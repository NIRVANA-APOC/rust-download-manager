//! JsonAction protocol: parse and validate the one JSON action a model may
//! emit per exploration turn (`docs/architecture.md` §11.9).
//!
//! Parsing is deliberately strict about *types* and about the security-relevant
//! fields (method allow-list, forbidden headers) but tolerates markdown fences
//! and prose around the object by reusing [`rdm_ai::extract_json_object`].
//! Unknown keys are ignored so a chatty model does not break the run.

use std::fmt;

use serde_json::{Map, Value};

use crate::contract::ResolveErrorKind;

/// Upper bound for one observed body (`docs/architecture.md` §11.9: 256 KiB).
pub(crate) const MAX_ACTION_BYTES: usize = 256 * 1024;

/// Header names exploration never forwards (no user credentials, ever).
pub(crate) const FORBIDDEN_HEADERS: [&str; 3] = ["cookie", "authorization", "proxy-authorization"];

/// One parsed model action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExploreAction {
    HttpRequest(HttpRequestAction),
    PageRead {
        url: String,
    },
    JsonQuery {
        obs_id: String,
        pointer: String,
    },
    RegexCapture {
        obs_id: String,
        pattern: String,
        group: Option<usize>,
    },
    VerifyResource {
        url: String,
        one_time: bool,
    },
    EmitManifest(EmitManifestAction),
    Finish {
        reason: String,
        kind: ResolveErrorKind,
    },
}

impl ExploreAction {
    /// Tool name used in traces and model feedback.
    pub(crate) fn op_name(&self) -> &'static str {
        match self {
            Self::HttpRequest(_) => "http_request",
            Self::PageRead { .. } => "page_read",
            Self::JsonQuery { .. } => "json_query",
            Self::RegexCapture { .. } => "regex_capture",
            Self::VerifyResource { .. } => "verify_resource",
            Self::EmitManifest(_) => "emit_manifest",
            Self::Finish { .. } => "finish",
        }
    }

    /// Redactable one-line description of the arguments (never a body).
    ///
    /// Traces run this through `crate::trace::redact_excerpt`; the digest is
    /// computed from the redacted form so signed query strings do not become
    /// guessable from a trace file.
    pub(crate) fn trace_summary(&self) -> String {
        match self {
            Self::HttpRequest(action) => format!("{} {}", action.method, action.url),
            Self::PageRead { url } => format!("GET {url}"),
            Self::JsonQuery { obs_id, pointer } => format!("json_query {obs_id} {pointer:?}"),
            Self::RegexCapture {
                obs_id,
                pattern,
                group,
            } => format!("regex_capture {obs_id} {pattern:?} group={group:?}"),
            Self::VerifyResource { url, one_time } => {
                format!("verify_resource {url} one_time={one_time}")
            }
            Self::EmitManifest(emit) => {
                format!("emit_manifest {} resource(s)", emit.resources.len())
            }
            Self::Finish { reason, kind } => format!("finish kind={kind:?} reason={reason:?}"),
        }
    }
}

/// `http_request` payload (only GET/HEAD/POST survive validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpRequestAction {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub max_bytes: usize,
}

/// One `emit_manifest.resources[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestCandidate {
    pub url: String,
    /// Trace handle: `obs_<id>`, `obs_<id>#/<json-pointer>` or
    /// `obs_<id>#links/<n>`.
    pub evidence: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub one_time: bool,
    pub single_stream: bool,
}

/// `emit_manifest` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmitManifestAction {
    pub resources: Vec<ManifestCandidate>,
    pub notes: Vec<String>,
}

/// Why an action could not be parsed (fed back to the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionError {
    pub message: String,
}

impl ActionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Payload of one evidence handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidenceRef {
    /// `obs_<id>`: the URL must equal the request/final URL, appear in the body
    /// text, or be one of the scanned page links.
    Observation,
    /// `obs_<id>#/<json-pointer>`: the pointer value must equal the URL.
    JsonPointer(String),
    /// `obs_<id>#links/<n>`: the n-th scanned link must equal the URL.
    Link(usize),
}

/// Split and syntactically validate an evidence handle.
///
/// Semantic checks (does the observation exist, does the URL really appear
/// there) happen against the live observation list in [`crate::explore`].
pub(crate) fn parse_evidence(evidence: &str) -> Result<(String, EvidenceRef), String> {
    let evidence = evidence.trim();
    if evidence.is_empty() {
        return Err("evidence must not be empty".to_owned());
    }
    let (obs_id, rest) = match evidence.split_once('#') {
        Some((obs_id, rest)) => (obs_id, Some(rest)),
        None => (evidence, None),
    };
    let obs_id = obs_id.trim();
    let Some(id) = obs_id.strip_prefix("obs_") else {
        return Err(format!(
            "evidence {evidence:?} must start with `obs_` (use the id returned by a tool)"
        ));
    };
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(format!(
            "evidence {evidence:?} has an invalid observation id"
        ));
    }
    let reference = match rest {
        None => EvidenceRef::Observation,
        Some("") => {
            return Err(format!(
                "evidence {evidence:?}: empty `#` fragment; drop the `#` or add a selector"
            ))
        }
        Some(rest) => {
            let rest = rest.trim();
            if let Some(index) = rest.strip_prefix("links/") {
                let index = index
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("evidence {evidence:?}: `links/<n>` needs a number"))?;
                EvidenceRef::Link(index)
            } else if rest.starts_with('/') {
                EvidenceRef::JsonPointer(rest.to_owned())
            } else {
                return Err(format!(
                    "evidence {evidence:?}: after `#` use `/<json-pointer>` or `links/<n>`"
                ));
            }
        }
    };
    Ok((obs_id.to_owned(), reference))
}

/// Parse one model reply into an action.
pub(crate) fn parse_action(reply: &str) -> Result<ExploreAction, ActionError> {
    let Some(json) = rdm_ai::extract_json_object(reply) else {
        return Err(ActionError::new(
            "reply contains no JSON object; answer with exactly one JSON action object",
        ));
    };
    let value: Value = serde_json::from_str(json)
        .map_err(|error| ActionError::new(format!("action JSON is invalid: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| ActionError::new("action must be a JSON object"))?;
    let action = required_str(object, "action")?;
    match action.as_str() {
        "http_request" => Ok(ExploreAction::HttpRequest(parse_http_request(object)?)),
        "page_read" => Ok(ExploreAction::PageRead {
            url: required_str(object, "url")?,
        }),
        "json_query" => {
            let obs_id = required_str(object, "obs_id")?;
            let pointer = required_str(object, "pointer")?;
            if !pointer.is_empty() && !pointer.starts_with('/') {
                return Err(ActionError::new(
                    "json_query.pointer must be a JSON Pointer starting with `/`",
                ));
            }
            Ok(ExploreAction::JsonQuery { obs_id, pointer })
        }
        "regex_capture" => {
            let obs_id = required_str(object, "obs_id")?;
            let pattern = required_str(object, "pattern")?;
            regex::Regex::new(&pattern).map_err(|error| {
                ActionError::new(format!(
                    "regex_capture.pattern is not a valid regex: {error}"
                ))
            })?;
            Ok(ExploreAction::RegexCapture {
                obs_id,
                pattern,
                group: optional_usize(object, "group")?,
            })
        }
        "verify_resource" => Ok(ExploreAction::VerifyResource {
            url: required_str(object, "url")?,
            one_time: optional_bool(object, "one_time")?.unwrap_or(false),
        }),
        "emit_manifest" => Ok(ExploreAction::EmitManifest(parse_emit_manifest(object)?)),
        "finish" => {
            let reason = required_str(object, "reason")?;
            let kind = match object.get("kind") {
                None | Some(Value::Null) => ResolveErrorKind::NoResources,
                Some(Value::String(name)) => {
                    ResolveErrorKind::from_name(name).ok_or_else(|| {
                        ActionError::new(format!(
                            "finish.kind {name:?} is not a valid ResolveErrorKind name"
                        ))
                    })?
                }
                Some(_) => {
                    return Err(ActionError::new("finish.kind must be a string"));
                }
            };
            Ok(ExploreAction::Finish { reason, kind })
        }
        other => Err(ActionError::new(format!(
            "unknown action {other:?}; allowed: http_request, page_read, json_query, \
             regex_capture, verify_resource, emit_manifest, finish"
        ))),
    }
}

fn parse_http_request(object: &Map<String, Value>) -> Result<HttpRequestAction, ActionError> {
    let method = required_str(object, "method")?.to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "HEAD" | "POST") {
        return Err(ActionError::new(format!(
            "http_request.method {method:?} is not allowed; use GET, HEAD or POST"
        )));
    }
    let url = required_str(object, "url")?;
    let body = match object.get("body") {
        None | Some(Value::Null) => None,
        Some(Value::String(body)) => Some(body.clone()),
        Some(_) => return Err(ActionError::new("http_request.body must be a string")),
    };
    let mut headers = Vec::new();
    if let Some(value) = object.get("headers") {
        if !value.is_null() {
            let map = value
                .as_object()
                .ok_or_else(|| ActionError::new("http_request.headers must be an object"))?;
            for (name, value) in map {
                let Some(value) = value.as_str() else {
                    return Err(ActionError::new(format!(
                        "http_request.headers.{name} must be a string"
                    )));
                };
                let lowered = name.to_ascii_lowercase();
                if FORBIDDEN_HEADERS.contains(&lowered.as_str()) {
                    return Err(ActionError::new(format!(
                        "http_request.headers.{name} is forbidden: exploration never carries \
                         credentials (cookie/authorization/proxy-authorization)"
                    )));
                }
                headers.push((name.clone(), value.to_owned()));
            }
        }
    }
    let max_bytes = match optional_usize(object, "max_bytes")? {
        Some(0) => return Err(ActionError::new("http_request.max_bytes must be positive")),
        Some(bytes) => bytes.min(MAX_ACTION_BYTES),
        None => MAX_ACTION_BYTES,
    };
    Ok(HttpRequestAction {
        method,
        url,
        headers,
        body,
        max_bytes,
    })
}

fn parse_emit_manifest(object: &Map<String, Value>) -> Result<EmitManifestAction, ActionError> {
    let resources = object
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| ActionError::new("emit_manifest.resources must be an array"))?;
    if resources.is_empty() {
        return Err(ActionError::new(
            "emit_manifest.resources must not be empty; use finish when nothing is downloadable",
        ));
    }
    let mut candidates = Vec::with_capacity(resources.len());
    for (index, resource) in resources.iter().enumerate() {
        let resource = resource.as_object().ok_or_else(|| {
            ActionError::new(format!(
                "emit_manifest.resources[{index}] must be an object"
            ))
        })?;
        let url = required_str(resource, "url")?;
        let evidence = required_str(resource, "evidence")?;
        parse_evidence(&evidence)
            .map_err(|error| ActionError::new(format!("resources[{index}].evidence: {error}")))?;
        candidates.push(ManifestCandidate {
            url,
            evidence,
            name: optional_str(resource, "name")?,
            kind: optional_str(resource, "kind")?,
            one_time: optional_bool(resource, "one_time")?.unwrap_or(false),
            single_stream: optional_bool(resource, "single_stream")?.unwrap_or(false),
        });
    }
    let notes = match object.get("notes") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let notes = value
                .as_array()
                .ok_or_else(|| ActionError::new("emit_manifest.notes must be an array"))?;
            notes
                .iter()
                .map(|note| {
                    note.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| ActionError::new("emit_manifest.notes must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(EmitManifestAction {
        resources: candidates,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Strict field helpers
// ---------------------------------------------------------------------------

fn required_str(object: &Map<String, Value>, field: &str) -> Result<String, ActionError> {
    match object.get(field) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
        Some(Value::String(_)) => Err(ActionError::new(format!("{field} must not be empty"))),
        Some(_) => Err(ActionError::new(format!("{field} must be a string"))),
        None => Err(ActionError::new(format!(
            "missing required field {field:?}"
        ))),
    }
}

fn optional_str(object: &Map<String, Value>, field: &str) -> Result<Option<String>, ActionError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ActionError::new(format!("{field} must be a string"))),
    }
}

fn optional_bool(object: &Map<String, Value>, field: &str) -> Result<Option<bool>, ActionError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ActionError::new(format!("{field} must be a boolean"))),
    }
}

fn optional_usize(object: &Map<String, Value>, field: &str) -> Result<Option<usize>, ActionError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| ActionError::new(format!("{field} must be a non-negative integer"))),
        Some(_) => Err(ActionError::new(format!(
            "{field} must be a non-negative integer"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(reply: &str) -> ExploreAction {
        parse_action(reply).expect("action must parse")
    }

    fn parse_err(reply: &str) -> String {
        parse_action(reply)
            .expect_err("action must be rejected")
            .message
    }

    #[test]
    fn parses_every_tool_action() {
        assert_eq!(
            parse(r#"{"action":"page_read","url":"https://example.com/p"}"#),
            ExploreAction::PageRead {
                url: "https://example.com/p".to_owned()
            }
        );
        assert_eq!(
            parse(r#"{"action":"json_query","obs_id":"obs_1","pointer":"/file/url"}"#),
            ExploreAction::JsonQuery {
                obs_id: "obs_1".to_owned(),
                pointer: "/file/url".to_owned()
            }
        );
        match parse(
            r#"{"action":"regex_capture","obs_id":"obs_1","pattern":"href=\"([^\"]+)\"","group":1}"#,
        ) {
            ExploreAction::RegexCapture { group, .. } => assert_eq!(group, Some(1)),
            other => panic!("unexpected action {other:?}"),
        }
        assert_eq!(
            parse(r#"{"action":"verify_resource","url":"https://x/y","one_time":true}"#),
            ExploreAction::VerifyResource {
                url: "https://x/y".to_owned(),
                one_time: true
            }
        );
        match parse(r#"{"action":"finish","reason":"nothing here"}"#) {
            ExploreAction::Finish { reason, kind } => {
                assert_eq!(reason, "nothing here");
                assert_eq!(kind, ResolveErrorKind::NoResources);
            }
            other => panic!("unexpected action {other:?}"),
        }
        match parse(r#"{"action":"finish","reason":"blocked","kind":"forbidden"}"#) {
            ExploreAction::Finish { kind, .. } => {
                assert_eq!(kind, ResolveErrorKind::Forbidden);
            }
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn parses_http_request_with_defaults_and_clamps() {
        let ExploreAction::HttpRequest(request) =
            parse(r#"{"action":"http_request","method":"get","url":"https://x/api"}"#)
        else {
            panic!("expected http_request");
        };
        assert_eq!(request.method, "GET");
        assert_eq!(request.max_bytes, MAX_ACTION_BYTES);
        assert!(request.headers.is_empty());
        assert!(request.body.is_none());

        let ExploreAction::HttpRequest(request) = parse(
            r#"{"action":"http_request","method":"POST","url":"https://x/api","headers":{"Accept":"application/json"},"body":"q=1","max_bytes":9999999}"#,
        ) else {
            panic!("expected http_request");
        };
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.body.as_deref(), Some("q=1"));
        assert_eq!(request.max_bytes, MAX_ACTION_BYTES);
    }

    #[test]
    fn parses_emit_manifest_with_evidence_forms() {
        let ExploreAction::EmitManifest(emit) = parse(
            r#"{"action":"emit_manifest","resources":[
                 {"url":"https://cdn.example.com/a.bin","evidence":"obs_1"}, 
                 {"url":"https://cdn.example.com/b.bin","evidence":"obs_2#/file/url","name":"b.bin"},
                 {"url":"https://cdn.example.com/c.bin","evidence":"obs_3#links/2","one_time":true,"single_stream":true}
               ],"notes":["done"]}"#,
        ) else {
            panic!("expected emit_manifest");
        };
        assert_eq!(emit.resources.len(), 3);
        assert_eq!(emit.resources[0].evidence, "obs_1");
        assert_eq!(emit.resources[1].name.as_deref(), Some("b.bin"));
        assert!(emit.resources[2].one_time);
        assert!(emit.resources[2].single_stream);
        assert_eq!(emit.notes, vec!["done".to_owned()]);
    }

    #[test]
    fn tolerates_fences_and_surrounding_prose() {
        let action = parse(
            "Sure, next step:\n```json\n{\"action\":\"page_read\",\"url\":\"https://x/p\"}\n```\ndone",
        );
        assert!(matches!(action, ExploreAction::PageRead { .. }));
    }

    #[test]
    fn rejects_unknown_actions_and_bad_types() {
        assert!(parse_err("not json at all").contains("no JSON object"));
        assert!(parse_err(r#"{"action":"run_script"}"#).contains("unknown action"));
        assert!(parse_err(r#"{"action":"page_read"}"#).contains("missing required field"));
        assert!(parse_err(r#"{"action":42}"#).contains("action must be a string"));
        assert!(
            parse_err(r#"{"action":"http_request","method":"DELETE","url":"https://x"}"#)
                .contains("not allowed")
        );
        assert!(
            parse_err(r#"{"action":"http_request","method":"GET","url":1}"#)
                .contains("must be a string")
        );
        assert!(parse_err(
            r#"{"action":"http_request","method":"GET","url":"https://x","max_bytes":"big"}"#
        )
        .contains("non-negative integer"));
        assert!(
            parse_err(r#"{"action":"json_query","obs_id":"obs_1","pointer":"file"}"#)
                .contains("JSON Pointer")
        );
        assert!(
            parse_err(r#"{"action":"regex_capture","obs_id":"obs_1","pattern":"("}"#)
                .contains("not a valid regex")
        );
        assert!(parse_err(r#"{"action":"finish"}"#).contains("missing required field"));
        assert!(
            parse_err(r#"{"action":"finish","reason":"x","kind":"made_up"}"#)
                .contains("not a valid ResolveErrorKind")
        );
    }

    #[test]
    fn forbids_credential_headers() {
        for header in ["Cookie", "authorization", "PROXY-AUTHORIZATION"] {
            let reply = format!(
                r#"{{"action":"http_request","method":"GET","url":"https://x","headers":{{"{header}":"secret"}}}}"#
            );
            let message = parse_err(&reply);
            assert!(
                message.contains("forbidden") && message.contains("credentials"),
                "{message}"
            );
        }
    }

    #[test]
    fn emit_manifest_requires_url_evidence_and_bounded_resource_count() {
        let message = parse_err(r#"{"action":"emit_manifest","resources":[]}"#);
        assert!(message.contains("must not be empty"), "{message}");
        let message =
            parse_err(r#"{"action":"emit_manifest","resources":[{"url":"https://x/a.bin"}]}"#);
        assert!(message.contains("evidence"), "{message}");
        let message = parse_err(
            r#"{"action":"emit_manifest","resources":[{"url":"https://x/a.bin","evidence":"nope"}]}"#,
        );
        assert!(message.contains("must start with `obs_`"), "{message}");
        let message = parse_err(
            r#"{"action":"emit_manifest","resources":[{"url":"https://x/a.bin","evidence":"obs_1#links/x"}]}"#,
        );
        assert!(message.contains("needs a number"), "{message}");
        let message = parse_err(
            r#"{"action":"emit_manifest","resources":[{"url":"https://x/a.bin","evidence":"obs_1#elsewhere"}]}"#,
        );
        assert!(message.contains("json-pointer"), "{message}");
    }

    #[test]
    fn evidence_syntax_is_parsed() {
        assert_eq!(
            parse_evidence("obs_12_0").unwrap(),
            ("obs_12_0".to_owned(), EvidenceRef::Observation)
        );
        assert_eq!(
            parse_evidence(" obs_12_0 ").unwrap(),
            ("obs_12_0".to_owned(), EvidenceRef::Observation)
        );
        assert_eq!(
            parse_evidence("obs_12_0#/file/url").unwrap(),
            (
                "obs_12_0".to_owned(),
                EvidenceRef::JsonPointer("/file/url".to_owned())
            )
        );
        assert_eq!(
            parse_evidence("obs_12_0#links/3").unwrap(),
            ("obs_12_0".to_owned(), EvidenceRef::Link(3))
        );
        assert!(parse_evidence("").is_err());
        assert!(parse_evidence("obs_").is_err());
        assert!(parse_evidence("obs_1#").is_err());
    }
}
