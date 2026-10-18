//! MCP stdio server: newline-delimited JSON-RPC 2.0 on stdin/stdout.
//!
//! * Legacy era (`initialize`, protocol ≤ 2025-11-25) is the primary path
//!   required by the RDM contract (`docs/agent-integration.md` §3).
//! * A **dual-era** layer is isolated here as additive handling:
//!   `server/discover`, per-request modern `_meta` (protocol version +
//!   client capabilities) and `resultType: "complete"` on modern results.
//!   Legacy requests/results are unchanged.
//!
//! Requests are processed concurrently: `tools/call` is spawned on the tokio
//! runtime, every response is correlated by JSON-RPC id, and a client
//! `notifications/cancelled` only aborts the *wait*, never the task.

use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rdm_agent::{ApprovalTokenRecord, DownloadPlan, Policy, TaskStore, LATEST_PROTOCOL_VERSION};
use serde_json::{json, Value};

use crate::manager::TaskManager;
use crate::protocol::{self, id_key, Message, Request, RpcError};
use crate::tools::CallContext;

/// Modern (2026-07-28) protocol version supported by the dual-era layer.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// Modern: unsupported protocol version (data.supported + data.requested).
pub const MODERN_UNSUPPORTED_VERSION: i64 = -32022;

pub const INSTRUCTIONS: &str = "\
RDM is a durable multi-connection download manager. Typical flow: rdm_capabilities -> \
rdm_probe(url) -> rdm_download(url, wait=true), or for files over the approval threshold \
rdm_plan_download -> rdm_apply_plan -> rdm_wait_task -> rdm_verify_file. Tasks are durable: \
rdm_list_tasks / rdm_get_task survive restarts and work across CLI/MCP processes; use \
rdm_wait_task instead of polling. Policy: destinations must resolve inside policy.allowed_dirs, \
hosts must match policy.allowed_hosts, and loopback/private/cloud-metadata hosts are rejected \
unless the policy explicitly enables allow_private_hosts. Files above require_approval_over_bytes \
return needs_approval with a single-use token (two-step confirmation). pause/cancel are control \
flow, not failures. All outputs and logs are redacted: cookies/authorization never appear.";

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Serialized stdout writer. Every MCP message is one line; failures (broken
/// pipe) are ignored so a dead client cannot abort task persistence.
pub struct Writer {
    out: Mutex<BufWriter<std::io::Stdout>>,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(BufWriter::new(std::io::stdout())),
        }
    }

    pub fn send(&self, message: &Value) {
        let Ok(line) = serde_json::to_string(message) else {
            tracing::error!("failed to serialize an MCP message");
            return;
        };
        let mut out = self.out.lock().expect("stdout writer poisoned");
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            // Client gone; nothing useful to do here.
        }
    }

    pub fn response(&self, id: Value, result: Value) {
        self.send(&protocol::success_response(id, result));
    }

    pub fn error_response(&self, id: Option<Value>, error: &RpcError) {
        self.send(&protocol::error_response(id, error));
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(&protocol::notification(method, params));
    }

    pub fn flush(&self) {
        let _ = self.out.lock().expect("stdout writer poisoned").flush();
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct ApprovalStore {
    tokens: HashMap<String, ApprovalTokenRecord>,
}

impl ApprovalStore {
    fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    fn prune(&mut self) {
        self.tokens.retain(|_, record| !record.is_expired());
    }
}

pub struct ServerState {
    pub writer: Arc<Writer>,
    pub state_root: PathBuf,
    pub store: TaskStore,
    pub policy: Policy,
    pub manager: Arc<TaskManager>,
    pub runtime: tokio::runtime::Handle,
    pub initialized: AtomicBool,
    /// Set once a valid modern request/discover is observed. Later requests
    /// without the required per-request `_meta` are modern attempts and must
    /// fail with `-32602`, not the legacy "not initialized" `-32002`.
    pub modern_seen: AtomicBool,
    pub shutdown: AtomicBool,
    pub client_protocol: Mutex<Option<String>>,
    pub client_info: Mutex<Option<Value>>,
    pub logging_level: Mutex<String>,
    wait_cancels: Mutex<HashMap<String, Weak<AtomicBool>>>,
    approvals: Mutex<ApprovalStore>,
}

impl ServerState {
    pub fn new(state_root: PathBuf, runtime: tokio::runtime::Handle, writer: Arc<Writer>) -> Self {
        let store = TaskStore::new(state_root.clone());
        let policy = Policy::load_or_default(&state_root);
        let manager = Arc::new(TaskManager::new(
            store.clone(),
            runtime.clone(),
            policy.clone(),
        ));
        let writer_for_notifications = Arc::clone(&writer);
        manager.set_notifier(Arc::new(move |method, params| {
            // Task lifecycle notifications are contract §3.4 `notifications/message`.
            let _ = &writer_for_notifications;
            writer_for_notifications.notify(method, params);
        }));
        Self {
            writer,
            state_root,
            store,
            policy,
            manager,
            runtime,
            initialized: AtomicBool::new(false),
            modern_seen: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            client_protocol: Mutex::new(None),
            client_info: Mutex::new(None),
            logging_level: Mutex::new("info".to_string()),
            wait_cancels: Mutex::new(HashMap::new()),
            approvals: Mutex::new(ApprovalStore::new()),
        }
    }

    /// Create a single-use approval token bound to `plan`.
    pub fn issue_approval(&self, plan: &DownloadPlan) -> String {
        let record = ApprovalTokenRecord::for_plan(plan);
        let token = record.token.clone();
        let mut approvals = self.approvals.lock().expect("approvals poisoned");
        approvals.prune();
        approvals.tokens.insert(token.clone(), record);
        token
    }

    pub fn consume_approval(
        &self,
        plan_id: &str,
        token: &str,
    ) -> Result<(), rdm_agent::AgentErrorInfo> {
        let mut approvals = self.approvals.lock().expect("approvals poisoned");
        approvals.prune();
        match approvals.tokens.remove(token) {
            Some(record) if record.plan_id != plan_id => Err(rdm_agent::AgentErrorInfo::new(
                rdm_agent::ErrorCode::InvalidRequest,
                "approval token does not belong to this plan",
            )),
            Some(record) if record.is_expired() => Err(rdm_agent::AgentErrorInfo::new(
                rdm_agent::ErrorCode::NeedsApproval,
                "approval token expired; create a new plan",
            )),
            Some(_) => Ok(()),
            None => Err(rdm_agent::AgentErrorInfo::new(
                rdm_agent::ErrorCode::NeedsApproval,
                "approval token is invalid or already used; create a new plan",
            )),
        }
    }

    pub fn register_wait_cancel(&self, key: String, flag: &Arc<AtomicBool>) {
        self.wait_cancels
            .lock()
            .expect("wait cancels poisoned")
            .insert(key, Arc::downgrade(flag));
    }

    pub fn unregister_wait_cancel(&self, key: &str) {
        self.wait_cancels
            .lock()
            .expect("wait cancels poisoned")
            .remove(key);
    }

    fn cancel_wait(&self, request_id: &Value) {
        let key = id_key(request_id);
        let weak = self
            .wait_cancels
            .lock()
            .expect("wait cancels poisoned")
            .get(&key)
            .cloned();
        if let Some(flag) = weak.and_then(|weak| weak.upgrade()) {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    /// Explicit state root (overrides `RDM_AGENT_STATE_DIR` / LOCALAPPDATA).
    pub state_dir: Option<PathBuf>,
    /// `RUST_LOG`-style filter for the stderr log.
    pub log_level: Option<String>,
}

impl ServerConfig {
    pub fn resolve_state_root(&self) -> PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(TaskStore::default_root)
    }
}

/// Run the stdio server until stdin reaches EOF. On EOF all running tasks are
/// paused (best effort) so a restart can resume them from `.part` state.
pub fn run_stdio(config: ServerConfig) -> anyhow::Result<()> {
    let state_root = config.resolve_state_root();
    if let Err(error) = TaskStore::new(state_root.clone()).ensure_layout() {
        tracing::warn!(
            state_dir = %state_root.display(),
            %error,
            "could not create the agent state layout; tools will report store errors"
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("rdm-mcp")
        .build()?;
    let writer = Arc::new(Writer::new());
    let state = Arc::new(ServerState::new(
        state_root.clone(),
        runtime.handle().clone(),
        Arc::clone(&writer),
    ));
    tracing::info!(
        state_dir = %state_root.display(),
        tools = %candidate_tool_count(),
        "rdm-mcp ready on stdio"
    );

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                handle_line(&state, trimmed);
            }
            Err(error) => {
                tracing::error!(%error, "stdin read failed; shutting down");
                break;
            }
        }
    }

    state.shutdown.store(true, Ordering::SeqCst);
    tracing::info!("stdin closed; pausing running tasks before exit");
    runtime.block_on(state.manager.shutdown());
    writer.flush();
    // Dropping the runtime here would abort worker tasks; `shutdown` already
    // paused and persisted them. `shutdown_timeout` gives network teardown a
    // brief moment without blocking exit.
    runtime.shutdown_timeout(Duration::from_millis(250));
    Ok(())
}

fn candidate_tool_count() -> usize {
    rdm_agent::tool_definitions().len()
}

fn handle_line(state: &Arc<ServerState>, line: &str) {
    match protocol::parse_message(line) {
        Ok(Message::Request(request)) => handle_request(state, request),
        Ok(Message::Notification { method, params }) => {
            handle_notification(state, &method, &params)
        }
        Err(error) => state.writer.error_response(None, &error),
    }
}

// ---------------------------------------------------------------------------
// Era detection / dispatch
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    Legacy,
    Modern,
}

fn request_meta_version(params: &Value) -> Option<String> {
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Validate the modern per-request `_meta` block (M1).
fn validate_modern_meta(params: &Value) -> Result<(), RpcError> {
    let meta = params.get("_meta").ok_or_else(|| {
        RpcError::invalid_params(
            "modern requests require params._meta with protocolVersion and clientCapabilities",
        )
    })?;
    let version = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RpcError::invalid_params("missing _meta[\"io.modelcontextprotocol/protocolVersion\"]")
        })?;
    if version != MODERN_PROTOCOL_VERSION {
        return Err(RpcError::with_data(
            MODERN_UNSUPPORTED_VERSION,
            format!("unsupported protocol version: {version}"),
            json!({ "supported": [MODERN_PROTOCOL_VERSION], "requested": version }),
        ));
    }
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        return Err(RpcError::invalid_params(
            "missing _meta[\"io.modelcontextprotocol/clientCapabilities\"] object",
        ));
    }
    Ok(())
}

/// Add the modern per-response fields (`resultType`, `_meta.serverInfo`).
fn modern_wrap_result(mut result: Value) -> Value {
    if let Value::Object(map) = &mut result {
        map.insert("resultType".to_string(), json!("complete"));
        map.insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/serverInfo": {
                    "name": rdm_agent::SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        );
    }
    result
}

/// Era decision.
///
/// * `modern_seen`: this connection already performed a valid modern request.
/// * `legacy_initialized`: the connection completed the legacy `initialize`
///   handshake. A legacy-capable client keeps working after a modern probe;
///   a modern-only client that never initialized must send `_meta` on every
///   request (`-32602` otherwise, never the legacy `-32002`).
fn era_for_request_with(
    request: &Request,
    modern_seen: bool,
    legacy_initialized: bool,
) -> Result<Era, RpcError> {
    if request.method == "initialize" {
        return Ok(Era::Legacy);
    }
    // `_meta` is also a legal legacy carrier (e.g. `progressToken`), so only a
    // modern namespaced key marks a modern (attempted) request.
    let modern_meta_attempt = request
        .params
        .get("_meta")
        .and_then(Value::as_object)
        .map(|meta| {
            meta.keys()
                .any(|key| key.starts_with("io.modelcontextprotocol/"))
        })
        .unwrap_or(false);
    match request_meta_version(&request.params) {
        Some(version) if version == MODERN_PROTOCOL_VERSION => {
            validate_modern_meta(&request.params)?;
            Ok(Era::Modern)
        }
        Some(version) => Err(RpcError::with_data(
            MODERN_UNSUPPORTED_VERSION,
            format!("unsupported protocol version: {version}"),
            json!({ "supported": [MODERN_PROTOCOL_VERSION], "requested": version }),
        )),
        None => {
            if request.method == "server/discover"
                || modern_meta_attempt
                || (modern_seen && !legacy_initialized)
            {
                Err(RpcError::invalid_params(
                    "modern requests require params._meta with \
                     \"io.modelcontextprotocol/protocolVersion\" and \
                     \"io.modelcontextprotocol/clientCapabilities\"",
                ))
            } else {
                Ok(Era::Legacy)
            }
        }
    }
}

fn handle_request(state: &Arc<ServerState>, request: Request) {
    let modern_seen = state.modern_seen.load(Ordering::SeqCst);
    let era = match era_for_request_with(
        &request,
        modern_seen,
        state.initialized.load(Ordering::SeqCst),
    ) {
        Ok(era) => era,
        Err(error) => {
            state
                .writer
                .error_response(Some(request.id.clone()), &error);
            return;
        }
    };
    if era == Era::Modern {
        state.modern_seen.store(true, Ordering::SeqCst);
    }

    // Legacy lifecycle gate: everything except initialize/ping needs the
    // handshake. Modern requests are stateless and always accepted.
    if era == Era::Legacy
        && !state.initialized.load(Ordering::SeqCst)
        && !matches!(request.method.as_str(), "initialize" | "ping")
    {
        state
            .writer
            .error_response(Some(request.id.clone()), &RpcError::not_initialized());
        return;
    }

    match request.method.as_str() {
        "initialize" => handle_initialize(state, request),
        "ping" => {
            if era == Era::Modern {
                state.writer.error_response(
                    Some(request.id.clone()),
                    &RpcError::method_not_found("ping"),
                );
            } else {
                state.writer.response(request.id, json!({}));
            }
        }
        "server/discover" => handle_discover(state, request),
        "tools/list" => {
            let result = json!({ "tools": tool_catalog() });
            respond(state, request.id, result, era, ListCache::Public);
        }
        "tools/call" => handle_tools_call(state, request, era),
        "resources/list" => {
            let result = crate::resources::resources_list();
            respond(state, request.id, result, era, ListCache::Public);
        }
        "resources/templates/list" => {
            let result = crate::resources::resource_templates_list();
            respond(state, request.id, result, era, ListCache::Public);
        }
        "resources/read" => {
            let uri = request
                .params
                .get("uri")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(uri) = uri else {
                state.writer.error_response(
                    Some(request.id),
                    &RpcError::invalid_params("resources/read requires `uri`"),
                );
                return;
            };
            match crate::resources::read_resource(state, &uri) {
                Ok(mut result) => {
                    if era == Era::Modern {
                        if let Value::Object(map) = &mut result {
                            map.insert("resultType".into(), json!("complete"));
                            map.insert("ttlMs".into(), json!(60000));
                            map.insert("cacheScope".into(), json!("private"));
                            map.insert(
                                "_meta".into(),
                                json!({
                                    "io.modelcontextprotocol/serverInfo": {
                                        "name": rdm_agent::SERVER_NAME,
                                        "version": env!("CARGO_PKG_VERSION"),
                                    }
                                }),
                            );
                        }
                    }
                    state.writer.response(request.id, result);
                }
                Err(mut error) => {
                    if era == Era::Modern && error.code == protocol::RESOURCE_NOT_FOUND {
                        error = RpcError::invalid_params(error.message);
                    }
                    state.writer.error_response(Some(request.id), &error);
                }
            }
        }
        "prompts/list" => {
            let result = crate::resources::prompts_list();
            respond(state, request.id, result, era, ListCache::Public);
        }
        "prompts/get" => match crate::resources::get_prompt(&request.params) {
            Ok(result) => respond(state, request.id, result, era, ListCache::None),
            Err(error) => state.writer.error_response(Some(request.id), &error),
        },
        "logging/setLevel" => {
            if era == Era::Modern {
                state.writer.error_response(
                    Some(request.id.clone()),
                    &RpcError::method_not_found("logging/setLevel"),
                );
                return;
            }
            if let Some(level) = request.params.get("level").and_then(Value::as_str) {
                *state.logging_level.lock().expect("logging level poisoned") = level.to_string();
            }
            state.writer.response(request.id, json!({}));
        }
        other => {
            state
                .writer
                .error_response(Some(request.id), &RpcError::method_not_found(other));
        }
    }
}

#[derive(Clone, Copy)]
enum ListCache {
    None,
    Public,
}

fn respond(state: &Arc<ServerState>, id: Value, mut result: Value, era: Era, cache: ListCache) {
    if era == Era::Modern {
        if let Value::Object(map) = &mut result {
            map.insert("resultType".to_string(), json!("complete"));
            if matches!(cache, ListCache::Public) {
                map.insert("ttlMs".to_string(), json!(300_000));
                map.insert("cacheScope".to_string(), json!("public"));
            }
            map.insert(
                "_meta".to_string(),
                json!({
                    "io.modelcontextprotocol/serverInfo": {
                        "name": rdm_agent::SERVER_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }),
            );
        }
    }
    state.writer.response(id, result);
}

fn tool_catalog() -> Vec<Value> {
    rdm_agent::tool_definitions()
        .into_iter()
        .map(|tool| serde_json::to_value(tool).unwrap_or(Value::Null))
        .collect()
}

fn handle_initialize(state: &Arc<ServerState>, request: Request) {
    let client_version = request
        .params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    let negotiated = protocol::negotiate_protocol_version(client_version);
    if let Some(client_info) = request.params.get("clientInfo") {
        *state.client_info.lock().expect("client info poisoned") = Some(client_info.clone());
    }
    *state
        .client_protocol
        .lock()
        .expect("client protocol poisoned") = Some(negotiated.to_string());
    state.initialized.store(true, Ordering::SeqCst);
    let result = json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": { "listChanged": false },
            "resources": { "subscribe": false },
            "prompts": {},
            "logging": {},
        },
        "serverInfo": {
            "name": rdm_agent::SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    });
    tracing::info!(
        client_version,
        negotiated,
        client = %state
            .client_info
            .lock()
            .expect("client info poisoned")
            .as_ref()
            .map(|info| info.to_string())
            .unwrap_or_default(),
        "initialize"
    );
    state.writer.response(request.id, result);
}

fn handle_discover(state: &Arc<ServerState>, request: Request) {
    if let Err(error) = validate_modern_meta(&request.params) {
        state.writer.error_response(Some(request.id), &error);
        return;
    }
    let capabilities = json!({
        "tools": {},
        "resources": {},
        "prompts": {},
    });
    let result = json!({
        "resultType": "complete",
        "supportedVersions": [MODERN_PROTOCOL_VERSION],
        "capabilities": capabilities,
        "instructions": INSTRUCTIONS,
        "ttlMs": 3_600_000,
        "cacheScope": "public",
        "_meta": {
            "io.modelcontextprotocol/serverInfo": {
                "name": rdm_agent::SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    });
    state.writer.response(request.id, result);
}

fn handle_tools_call(state: &Arc<ServerState>, request: Request, era: Era) {
    let Some(name) = request
        .params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        state.writer.error_response(
            Some(request.id),
            &RpcError::invalid_params("tools/call requires `name`"),
        );
        return;
    };
    if rdm_agent::tool_by_name(&name).is_none() {
        state.writer.error_response(
            Some(request.id),
            &RpcError::invalid_params(format!("unknown tool: {name}")),
        );
        return;
    }
    let arguments = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let progress_token = request
        .params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
        .cloned();
    let wait_cancel = Arc::new(AtomicBool::new(false));
    let key = id_key(&request.id);
    state.register_wait_cancel(key.clone(), &wait_cancel);
    let ctx = CallContext {
        server: Arc::clone(state),
        call_id: request.id.clone(),
        progress_token,
        wait_cancel,
    };
    let id = request.id;
    let state_for_task = Arc::clone(state);
    state.runtime.spawn(async move {
        let mut result = crate::tools::call_tool(&ctx, &name, &arguments).await;
        if era == Era::Modern {
            result = modern_wrap_result(result);
        }
        state_for_task.writer.response(id, result);
        state_for_task.unregister_wait_cancel(&key);
    });
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

fn handle_notification(state: &Arc<ServerState>, method: &str, params: &Value) {
    match method {
        "notifications/initialized" => {
            state.initialized.store(true, Ordering::SeqCst);
        }
        "notifications/cancelled" => {
            if let Some(request_id) = params.get("requestId") {
                state.cancel_wait(request_id);
            }
        }
        "notifications/progress"
        | "notifications/roots/list_changed"
        | "notifications/resources/list_changed"
        | "notifications/prompts/list_changed" => {}
        other => {
            tracing::debug!(method = other, "ignoring unknown notification");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_cover_flow_and_safety() {
        assert!(INSTRUCTIONS.contains("rdm_capabilities"));
        assert!(INSTRUCTIONS.contains("rdm_apply_plan"));
        assert!(INSTRUCTIONS.contains("policy"));
        assert!(INSTRUCTIONS.contains("redacted"));
        assert!(INSTRUCTIONS.len() > 200);
    }

    #[test]
    fn modern_meta_is_validated() {
        let valid = json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        assert!(validate_modern_meta(&valid).is_ok());
        let missing = json!({});
        assert_eq!(
            validate_modern_meta(&missing).unwrap_err().code,
            protocol::INVALID_PARAMS
        );
        let wrong_version = json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-06-18",
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        });
        assert_eq!(
            validate_modern_meta(&wrong_version).unwrap_err().code,
            MODERN_UNSUPPORTED_VERSION
        );
        let missing_caps = json!({
            "_meta": { "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION }
        });
        assert_eq!(
            validate_modern_meta(&missing_caps).unwrap_err().code,
            protocol::INVALID_PARAMS
        );
    }

    #[test]
    fn era_detection_handles_legacy_and_modern() {
        let legacy = Request {
            id: json!(1),
            method: "tools/list".to_string(),
            params: json!({}),
        };
        assert_eq!(
            era_for_request_with(&legacy, false, false).unwrap(),
            Era::Legacy
        );
        let modern = Request {
            id: json!(2),
            method: "tools/list".to_string(),
            params: json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }),
        };
        assert_eq!(
            era_for_request_with(&modern, false, false).unwrap(),
            Era::Modern
        );
        let initialize = Request {
            id: json!(3),
            method: "initialize".to_string(),
            params: json!({}),
        };
        assert_eq!(
            era_for_request_with(&initialize, false, false).unwrap(),
            Era::Legacy
        );
        let discover_without_meta = Request {
            id: json!(4),
            method: "server/discover".to_string(),
            params: json!({}),
        };
        assert!(era_for_request_with(&discover_without_meta, false, false).is_err());
        let wrong = Request {
            id: json!(5),
            method: "tools/list".to_string(),
            params: json!({
                "_meta": { "io.modelcontextprotocol/protocolVersion": "2025-11-25" }
            }),
        };
        assert_eq!(
            era_for_request_with(&wrong, false, false).unwrap_err().code,
            MODERN_UNSUPPORTED_VERSION
        );
    }

    #[test]
    fn modern_missing_meta_is_invalid_params_not_legacy_not_initialized() {
        // A request without `_meta` after the connection has spoken modern is a
        // malformed modern request: -32602, and never the legacy -32002.
        let tools_without_meta = Request {
            id: json!(1),
            method: "tools/list".to_string(),
            params: json!({}),
        };
        let error = era_for_request_with(&tools_without_meta, true, false).unwrap_err();
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        assert_ne!(error.code, protocol::RESOURCE_NOT_FOUND);
        // Before any modern request it is a legacy client (backward compat).
        assert_eq!(
            era_for_request_with(&tools_without_meta, false, false).unwrap(),
            Era::Legacy
        );
        // A legacy client that already completed `initialize` keeps working
        // even after it probed the modern era (dual-era fallback).
        assert_eq!(
            era_for_request_with(&tools_without_meta, true, true).unwrap(),
            Era::Legacy
        );
        // `_meta` present but missing required fields is also -32602.
        let partial_meta = Request {
            id: json!(2),
            method: "tools/list".to_string(),
            params: json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION
                }
            }),
        };
        assert_eq!(
            era_for_request_with(&partial_meta, false, false)
                .unwrap_err()
                .code,
            protocol::INVALID_PARAMS
        );
        // server/discover without _meta stays -32602 in all cases.
        let discover = Request {
            id: json!(3),
            method: "server/discover".to_string(),
            params: json!({}),
        };
        assert_eq!(
            era_for_request_with(&discover, false, false)
                .unwrap_err()
                .code,
            protocol::INVALID_PARAMS
        );
        // Unknown versions still get -32022 with the supported list.
        let wrong = Request {
            id: json!(4),
            method: "tools/list".to_string(),
            params: json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }),
        };
        let error = era_for_request_with(&wrong, true, false).unwrap_err();
        assert_eq!(error.code, MODERN_UNSUPPORTED_VERSION);
        assert_eq!(
            error.data.as_ref().unwrap()["supported"][0],
            MODERN_PROTOCOL_VERSION
        );
        // Legacy `_meta` carriers (progressToken) must not be mistaken for a
        // malformed modern request.
        let progress_only = Request {
            id: json!(5),
            method: "tools/call".to_string(),
            params: json!({
                "name": "rdm_capabilities",
                "arguments": {},
                "_meta": { "progressToken": "p1" }
            }),
        };
        assert_eq!(
            era_for_request_with(&progress_only, false, false).unwrap(),
            Era::Legacy
        );
    }

    #[test]
    fn modern_results_get_result_type_and_server_info() {
        let result = modern_wrap_result(json!({ "ok": true }));
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            rdm_agent::SERVER_NAME
        );
    }

    #[test]
    fn tool_catalog_count_matches_registry() {
        assert_eq!(candidate_tool_count(), 16);
        let catalog = tool_catalog();
        assert_eq!(catalog.len(), 16);
        assert_eq!(catalog[0]["name"], "rdm_capabilities");
    }
}
