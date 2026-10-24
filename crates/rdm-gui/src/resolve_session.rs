//! 「资源解析会话」的纯状态与持久化（不依赖 `gpui`，可单元测试）。
//!
//! GUI 的「资源解析」页面把每次解析当成一个会话（类似聊天会话）：标题、来源 URL、
//! 消息、资源清单、状态、token 用量与时间都持久化到
//! `%LOCALAPPDATA%\RDM\resolve_sessions.json`，应用重启后历史仍在。
//!
//! 只持久化**可重建的展示数据**：
//! - 内存态不落盘：请求代号（`title_request` / `chat_request` / `agent_request` /
//!   `agent_summary_request` / `panel.request_id`）、流式状态、[`AgentPlan`]，以及
//!   [`ParseCredential`]（Cookie/Referer/UA 是机密，加载后恒为 `None`）；
//! - 上次退出时仍处于 `parsing` 的会话，加载时转为 `failed` 并写明原因——不能假装
//!   解析还在进行，也不能悄悄丢掉这条历史；
//! - 聊天消息与内存面板使用同一上限 [`AI_CHAT_HISTORY_LIMIT`]：保存与加载都只保留
//!   末尾部分，避免历史无限增长拖慢每帧渲染。
//!
//! 文件格式（`version = 1`）：
//!
//! ```json
//! {"version":1,"next_seq":3,"sessions":[{"id":"rs_000002","title":"…", "...": "..."}]}
//! ```
//!
//! `status` 在文件里是字符串（`idle|parsing|ready|failed`），`resources` 的元素是
//! `{"resource": ParsedResource, "selected": bool}`；`sessions` 按「新 → 旧」排列。
//! 「当前会话」（`active`）与 `next_seq` 一样属于本机展示状态：`next_seq` 持久化，
//! `active` 不持久化（加载时默认选中最新会话，等同于聊天应用恢复到最近一次对话）。
//!
//! 历史可编辑：`pinned`（置顶）与 `group`（分组）同样落盘（旧文件缺字段走默认值）；
//! 同一规范化页面 URL 的解析复用历史会话（[`normalize_session_url`] 是去重键），
//! 不再为同一页面重复创建会话。

#![allow(dead_code)] // 会话存储 API：视图层（`views/app.rs`）按需接入

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rdm_ai::{ParsedResource, TokenUsage};
use serde::{Deserialize, Serialize};

use crate::agent::AgentPlan;
use crate::model::{
    write_atomic, AiChatMessage, AiPanelState, AiParseStatus, AiResourceCard, AiRole,
    ParseUsageSnapshot, AI_CHAT_HISTORY_LIMIT,
};

/// 解析会话持久化文件名（位于 GUI state 目录）。
pub const RESOLVE_SESSIONS_FILE: &str = "resolve_sessions.json";
/// 解析会话持久化文件版本。
pub const RESOLVE_SESSIONS_VERSION: u32 = 1;
/// 「上次解析未完成」的固定错误文案（`parsing` 会话在重启后的恢复结果）。
pub const INTERRUPTED_PARSE_ERROR: &str = "上次解析未完成（应用已重启）";

/// 同一规范化页面 URL 命中历史会话、原地重新解析时追加的系统消息。
///
/// 桌面端输入与浏览器扩展 `ai_parse` 共用同一文案：用户必须能一眼看出「没有新建
/// 重复会话，而是复用了历史」。
pub const REUSE_SESSION_NOTICE: &str = "已复用历史解析会话（同一页面），本次重新解析";

/// 生成标题的上下文上限（避免长对话把标题 prompt 撑爆）。
const TITLE_PROMPT_MAX_NOTES: usize = 5;
const TITLE_PROMPT_MAX_RESOURCES: usize = 8;
const TITLE_PROMPT_MAX_USER_CHARS: usize = 200;
const TITLE_PROMPT_MAX_NOTE_CHARS: usize = 120;
/// 会话列表里 fallback 标题的总长度上限（含 ` · N 个资源` 后缀）。
const FALLBACK_TITLE_MAX_CHARS: usize = 40;
/// 无 URL 时取第一条用户消息的前缀长度。
const MESSAGE_TITLE_MAX_CHARS: usize = 24;
/// LLM 生成标题的最终长度上限（含省略号）。
const GENERATED_TITLE_MAX_CHARS: usize = 32;

/// 解析会话所在的持久化文件路径。
pub fn resolve_sessions_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RESOLVE_SESSIONS_FILE)
}

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

/// 会话来源：桌面端直接发起、浏览器扩展转发、Agent 工作流。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResolveOrigin {
    #[default]
    Gui,
    Extension,
    Agent,
}

impl ResolveOrigin {
    /// 会话来源的 UI 文案（历史列表徽标）。
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Gui => "桌面端",
            Self::Extension => "浏览器扩展",
            Self::Agent => "Agent",
        }
    }

    /// 会话创建的下载任务应写入的 `DownloadItem.source` token。
    ///
    /// 与 `views/app.rs::source_label/ViewFilter` 的既有词表保持一致：
    /// 桌面端 `gui` / 浏览器扩展 `extension` / Agent 工作流 `agent`。
    /// 这是「会话来源 → 下载来源」的唯一映射，避免各调用点硬编码（审计 F-05）。
    pub fn download_source(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Extension => "extension",
            Self::Agent => "agent",
        }
    }
}

/// 标题来源：LLM 生成（用户可读、已清洗）或确定性兜底（[`ResolveSession::fallback_title`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    Llm,
    #[default]
    Fallback,
}

/// 持久化 JSON 里的解析状态（人类可读字符串，`serde` 用 snake_case）。
///
/// 运行时的 [`AiParseStatus`] 没有实现 `Serialize`（它是纯 UI 状态机），
/// 这里显式做一层映射，保证文件里的 `status` 是稳定字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PersistedParseStatus {
    #[default]
    Idle,
    Parsing,
    Ready,
    Failed,
}

impl PersistedParseStatus {
    fn from_status(status: &AiParseStatus) -> Self {
        match status {
            AiParseStatus::Idle => Self::Idle,
            AiParseStatus::Parsing => Self::Parsing,
            AiParseStatus::Ready => Self::Ready,
            AiParseStatus::Failed => Self::Failed,
        }
    }

    fn to_status(self) -> AiParseStatus {
        match self {
            Self::Idle => AiParseStatus::Idle,
            Self::Parsing => AiParseStatus::Parsing,
            Self::Ready => AiParseStatus::Ready,
            Self::Failed => AiParseStatus::Failed,
        }
    }
}

/// 持久化 JSON 里的消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PersistedRole {
    #[default]
    System,
    User,
    Assistant,
}

impl PersistedRole {
    fn from_ai_role(role: AiRole) -> Self {
        match role {
            AiRole::System => Self::System,
            AiRole::User => Self::User,
            AiRole::Assistant => Self::Assistant,
        }
    }

    fn to_ai_role(self) -> AiRole {
        match self {
            Self::System => AiRole::System,
            Self::User => AiRole::User,
            Self::Assistant => AiRole::Assistant,
        }
    }
}

// ---------------------------------------------------------------------------
// 持久化载体
// ---------------------------------------------------------------------------

/// 持久化的一条聊天消息（不含流式状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedMessage {
    #[serde(default)]
    pub role: PersistedRole,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub created_ms: u64,
}

/// 持久化的一个候选资源（`selected` 是用户勾选状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedResource {
    pub resource: ParsedResource,
    #[serde(default = "default_selected")]
    pub selected: bool,
}

fn default_selected() -> bool {
    true
}

/// 一个解析会话的持久化形态。
///
/// 字段全部带默认值：旧版本文件缺字段时按「未解析 / 无消息」加载，而不是整份历史作废。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedResolveSession {
    pub id: String,
    pub title: String,
    pub title_source: TitleSource,
    /// 用户手动置顶（列表排序优先；旧文件缺字段 = false）。
    #[serde(default)]
    pub pinned: bool,
    /// 用户自定义分组名（None = 未分组；空字符串在加载时归一化为 None）。
    #[serde(default)]
    pub group: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub origin: ResolveOrigin,
    pub status: PersistedParseStatus,
    pub source_url: String,
    pub messages: Vec<PersistedMessage>,
    pub resources: Vec<PersistedResource>,
    pub notes: Vec<String>,
    pub ai_used: bool,
    pub cache_hit: bool,
    pub model: Option<String>,
    pub usage: TokenUsage,
    pub error: Option<String>,
}

/// 持久化文件根对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedResolveSessionsFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default = "default_next_seq")]
    next_seq: u64,
    #[serde(default)]
    sessions: Vec<PersistedResolveSession>,
}

fn default_version() -> u32 {
    RESOLVE_SESSIONS_VERSION
}

fn default_next_seq() -> u64 {
    1
}

// ---------------------------------------------------------------------------
// 会话
// ---------------------------------------------------------------------------

/// 一次「资源解析」会话：持久化展示数据 + 仅内存的请求/执行状态。
#[derive(Debug, Clone)]
pub struct ResolveSession {
    pub id: String,
    pub title: String,
    pub title_source: TitleSource,
    /// 是否置顶（持久化；列表排序 pinned 优先，其后 updated_ms 降序）。
    pub pinned: bool,
    /// 用户自定义分组名（持久化；`None` = 未分组）。
    pub group: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub origin: ResolveOrigin,
    pub panel: AiPanelState,
    // ---- 仅内存（不持久化）----
    /// Agent 工作流执行计划（步骤时间线；应用重启后无法恢复）。
    pub plan: Option<AgentPlan>,
    /// 标题生成请求代号。
    pub title_request: Option<u64>,
    /// 普通聊天/汇报流式请求 id。
    pub chat_request: Option<u64>,
    /// 工作流解析请求 id。
    pub agent_request: Option<u64>,
    /// 工作流汇报请求 id。
    pub agent_summary_request: Option<u64>,
    /// 本次会话最近一次解析的 token/费用快照（供随后由**本会话**创建的下载项归属；
    /// 审计 F-02：归属必须跟着会话走，不能用 RdmApp 上的全局槽位）。
    pub last_parse_usage: Option<ParseUsageSnapshot>,
    /// 本次解析是否是「重新解析（强制 AI）」（design 17 §2.5，仅内存）。
    ///
    /// 为 `true` 时解析结果与上次结果合并（保留更丰富的旧资源），失败也不清空资源；
    /// 解析结束/失败回填后由视图层复位。
    pub reparse_keep_best: bool,
    /// 重新解析开始前的资源快照（design 17 §2.5，仅内存）。
    ///
    /// `AiPanelState::begin_parse` 会清空 `panel.resources`，所以必须在开始解析**之前**
    /// 保存旧结果；解析完成/失败时再由视图层取出，按 URL 合并或用于「失败保留」。
    pub reparse_previous_resources: Option<Vec<AiResourceCard>>,
}

impl ResolveSession {
    /// 新建一个空会话（`idle`、无来源 URL、无消息），标题先给确定性兜底「新解析」。
    pub fn new(id: String, origin: ResolveOrigin, now_ms: u64) -> Self {
        let panel = AiPanelState::default();
        let title = fallback_title_for(&panel);
        Self {
            id,
            title,
            title_source: TitleSource::Fallback,
            pinned: false,
            group: None,
            created_ms: now_ms,
            updated_ms: now_ms,
            origin,
            panel,
            plan: None,
            title_request: None,
            chat_request: None,
            agent_request: None,
            agent_summary_request: None,
            last_parse_usage: None,
            reparse_keep_best: false,
            reparse_previous_resources: None,
        }
    }

    /// 刷新「最近更新时间」（解析/聊天有进展时由视图层调用）。
    pub fn touch(&mut self, now_ms: u64) {
        self.updated_ms = now_ms;
    }

    /// 解析状态的 UI 文案（与 `model::AiParseStatus::label_zh` 一致）。
    pub fn status_label_zh(&self) -> &'static str {
        match &self.panel.status {
            AiParseStatus::Idle => "未解析",
            AiParseStatus::Parsing => "解析中…",
            AiParseStatus::Ready => "已解析",
            AiParseStatus::Failed => "解析失败",
        }
    }

    /// 候选资源数量（不与「已勾选数量」混淆；后者用 `panel.selected_count()`）。
    pub fn resource_count(&self) -> usize {
        self.panel.resources.len()
    }

    /// 是否有一次解析结果（成功或失败都算；`idle`/`parsing` 不算）。
    pub fn has_parse_result(&self) -> bool {
        matches!(
            self.panel.status,
            AiParseStatus::Ready | AiParseStatus::Failed
        )
    }

    /// 是否还需要 LLM 生成一个更好的标题：当前是兜底标题，且已经出了解析结果。
    pub fn needs_title(&self) -> bool {
        self.title_source == TitleSource::Fallback && self.has_parse_result()
    }

    /// 确定性兜底标题（不调用 AI）：
    ///
    /// 1. 有来源 URL → `host · N 个资源`；解析失败（或没有资源）→ `host · 解析失败`；
    ///    取不到 host 时用 URL 本身（截断）；
    /// 2. 否则取第一条用户消息的前 [`MESSAGE_TITLE_MAX_CHARS`] 个字符；
    /// 3. 否则「新解析」。
    ///
    /// 结果总长度不超过 [`FALLBACK_TITLE_MAX_CHARS`] 个字符（按字符，不切断中文/emoji）。
    pub fn fallback_title(&self) -> String {
        fallback_title_for(&self.panel)
    }

    /// 给 LLM 生成标题的 prompt；没有任何可用上下文时返回 `None`。
    ///
    /// 上下文（全部有界）：来源 URL、页面笔记前 5 条、候选资源前 8 个（name + kind）、
    /// 第一条用户消息（≤200 字）。指令要求只输出一个 ≤20 字的标题，不要引号/编号/解释。
    pub fn title_prompt(&self) -> Option<String> {
        let panel = &self.panel;
        let source_url = panel.source_url.trim();
        let first_user = panel
            .messages
            .iter()
            .find(|message| message.role == AiRole::User)
            .map(|message| truncate_chars(&collapse_whitespace(message.text.trim()), 200))
            .filter(|text| !text.is_empty());
        let notes: Vec<&str> = panel
            .notes
            .iter()
            .map(|note| note.trim())
            .filter(|note| !note.is_empty())
            .take(TITLE_PROMPT_MAX_NOTES)
            .collect();
        let resources: Vec<(&str, &str)> = panel
            .resources
            .iter()
            .take(TITLE_PROMPT_MAX_RESOURCES)
            .map(|card| (card.resource.name.trim(), card.resource.kind.trim()))
            .collect();
        if source_url.is_empty() && first_user.is_none() && notes.is_empty() && resources.is_empty()
        {
            return None;
        }

        let mut prompt = String::from(
            "你是 RDM 下载管理器的会话标题生成器。根据下面的解析上下文，只输出一个 ≤20 字的会话标题，\
             不要引号、不要编号、不要解释、不要换行。\n\n",
        );
        if !source_url.is_empty() {
            prompt.push_str("来源 URL：");
            prompt.push_str(source_url);
            prompt.push('\n');
        }
        if !notes.is_empty() {
            prompt.push_str("页面笔记（前 5 条）：\n");
            for note in notes {
                prompt.push_str("- ");
                prompt.push_str(&truncate_chars(note, TITLE_PROMPT_MAX_NOTE_CHARS));
                prompt.push('\n');
            }
        }
        if !resources.is_empty() {
            prompt.push_str("候选资源（前 8 个）：\n");
            for (name, kind) in resources {
                prompt.push_str("- ");
                prompt.push_str(name);
                prompt.push('（');
                prompt.push_str(kind);
                prompt.push_str("）\n");
            }
        }
        if let Some(text) = first_user {
            prompt.push_str("第一条用户消息：");
            prompt.push_str(&text);
            prompt.push('\n');
        }
        Some(prompt.trim_end().to_string())
    }

    /// 应用 LLM 生成的标题：清洗后写回并标记来源为 [`TitleSource::Llm`]。
    ///
    /// 清洗规则：只取第一行、去 markdown/列表/编号前缀与包裹引号书名号、压缩空白、
    /// 截断到 [`GENERATED_TITLE_MAX_CHARS`]（超出以 `…` 结尾）；清洗后为空 → 返回
    /// `false` 且不改动任何字段。成功时同步刷新 `updated_ms`。
    pub fn apply_generated_title(&mut self, raw: &str, now_ms: u64) -> bool {
        let Some(title) = sanitize_generated_title(raw) else {
            return false;
        };
        self.title = title;
        self.title_source = TitleSource::Llm;
        self.touch(now_ms);
        true
    }

    /// 转成持久化形态（消息按上限只保留末尾部分；内存态字段全部丢弃）。
    pub fn to_persisted(&self) -> PersistedResolveSession {
        let start = self
            .panel
            .messages
            .len()
            .saturating_sub(AI_CHAT_HISTORY_LIMIT);
        PersistedResolveSession {
            id: self.id.clone(),
            title: self.title.clone(),
            title_source: self.title_source,
            pinned: self.pinned,
            group: self.group.clone(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
            origin: self.origin,
            status: PersistedParseStatus::from_status(&self.panel.status),
            source_url: self.panel.source_url.clone(),
            messages: self.panel.messages[start..]
                .iter()
                .map(|message| PersistedMessage {
                    role: PersistedRole::from_ai_role(message.role),
                    text: message.text.clone(),
                    created_ms: message.created_ms,
                })
                .collect(),
            resources: self
                .panel
                .resources
                .iter()
                .map(|card| PersistedResource {
                    resource: card.resource.clone(),
                    selected: card.selected,
                })
                .collect(),
            notes: self.panel.notes.clone(),
            ai_used: self.panel.ai_used,
            cache_hit: self.panel.cache_hit,
            model: self.panel.model.clone(),
            usage: self.panel.usage,
            error: self.panel.error.clone(),
        }
    }

    /// 从持久化形态恢复：请求代号/凭据/plan/流式状态全部置空；
    /// 上次未完成的 `parsing` 会话转为 `failed` 并写明原因；消息只保留末尾上限条。
    pub fn from_persisted(persisted: PersistedResolveSession) -> Self {
        let mut status = persisted.status.to_status();
        let mut error = persisted.error;
        if status == AiParseStatus::Parsing {
            status = AiParseStatus::Failed;
            error = Some(INTERRUPTED_PARSE_ERROR.to_string());
        }
        // 旧文件/手改文件里的空白分组按「未分组」处理，与 `set_group` 同一归一化。
        let group = persisted.group.as_deref().and_then(normalize_session_group);

        let mut messages: Vec<AiChatMessage> = persisted
            .messages
            .into_iter()
            .map(|message| AiChatMessage {
                role: message.role.to_ai_role(),
                text: message.text,
                created_ms: message.created_ms,
            })
            .collect();
        if messages.len() > AI_CHAT_HISTORY_LIMIT {
            let excess = messages.len() - AI_CHAT_HISTORY_LIMIT;
            messages.drain(..excess);
        }

        let panel = AiPanelState {
            visible: false,
            status,
            source_url: persisted.source_url,
            messages,
            resources: persisted
                .resources
                .into_iter()
                .map(|saved| AiResourceCard {
                    resource: saved.resource,
                    selected: saved.selected,
                })
                .collect(),
            notes: persisted.notes,
            ai_used: persisted.ai_used,
            cache_hit: persisted.cache_hit,
            model: persisted.model,
            usage: persisted.usage,
            error,
            credential: None,
            request_id: 0,
            chat_pending: false,
            streaming: false,
            last_plan_summary: None,
        };
        Self {
            id: persisted.id,
            title: persisted.title,
            title_source: persisted.title_source,
            pinned: persisted.pinned,
            group,
            created_ms: persisted.created_ms,
            updated_ms: persisted.updated_ms,
            origin: persisted.origin,
            panel,
            plan: None,
            title_request: None,
            chat_request: None,
            agent_request: None,
            agent_summary_request: None,
            last_parse_usage: None,
            reparse_keep_best: false,
            reparse_previous_resources: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 会话集合
// ---------------------------------------------------------------------------

/// 解析会话集合：内存中的「新 → 旧」列表 + 当前会话 + 持久化脏标记。
///
/// 视图层修改任何会被持久化的字段后调用 [`ResolveStore::mark_dirty`]；
/// 引擎/定时器用 [`ResolveStore::save_if_dirty`] 落盘（无变化不写文件）。
#[derive(Debug)]
pub struct ResolveStore {
    sessions: Vec<ResolveSession>,
    /// 当前打开的会话 id（不持久化；`None` = 「新建解析」空态）。
    active: Option<String>,
    next_seq: u64,
    dirty: bool,
}

impl Default for ResolveStore {
    fn default() -> Self {
        Self::empty()
    }
}

impl ResolveStore {
    fn empty() -> Self {
        Self {
            sessions: Vec::new(),
            active: None,
            next_seq: 1,
            dirty: false,
        }
    }

    /// 从 `state_dir/resolve_sessions.json` 加载历史；文件缺失/损坏 → 空 store。
    ///
    /// 损坏只 `tracing::warn` 并放弃这份文件（下次保存会覆盖），绝不 panic ——
    /// 解析历史是展示数据，启动失败不能拖垮 GUI。
    pub fn load(state_dir: &Path) -> Self {
        let path = resolve_sessions_path(state_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::empty(),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "resolve_sessions.json 读取失败，解析历史按空处理"
                );
                return Self::empty();
            }
        };
        let file: PersistedResolveSessionsFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "resolve_sessions.json 解析失败，解析历史按空处理"
                );
                return Self::empty();
            }
        };
        if file.version != RESOLVE_SESSIONS_VERSION {
            tracing::warn!(
                path = %path.display(),
                version = file.version,
                expected = RESOLVE_SESSIONS_VERSION,
                "resolve_sessions.json 版本不匹配，按尽力而为加载"
            );
        }

        let mut sessions: Vec<ResolveSession> = Vec::with_capacity(file.sessions.len());
        for persisted in file.sessions {
            if persisted.id.trim().is_empty() {
                tracing::warn!("resolve_sessions.json 中存在 id 为空的会话，已跳过");
                continue;
            }
            sessions.push(ResolveSession::from_persisted(persisted));
        }

        let max_seq = sessions
            .iter()
            .filter_map(|session| session_seq(&session.id))
            .max()
            .unwrap_or(0);
        let next_seq = file.next_seq.max(max_seq + 1).max(1);
        // 「当前会话」不落盘：默认恢复到最新一条（等价于聊天应用打开上次对话）。
        let active = sessions.first().map(|session| session.id.clone());
        Self {
            sessions,
            active,
            next_seq,
            dirty: false,
        }
    }

    /// 有变化才落盘；返回是否真的写了文件。
    pub fn save_if_dirty(&mut self, state_dir: &Path) -> std::io::Result<bool> {
        if !self.dirty {
            return Ok(false);
        }
        self.save_now(state_dir)?;
        self.dirty = false;
        Ok(true)
    }

    /// 无条件落盘（不改变脏标记；正常路径用 [`Self::save_if_dirty`]）。
    pub fn save_now(&self, state_dir: &Path) -> std::io::Result<()> {
        let file = PersistedResolveSessionsFile {
            version: RESOLVE_SESSIONS_VERSION,
            next_seq: self.next_seq,
            sessions: self
                .sessions
                .iter()
                .map(ResolveSession::to_persisted)
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        write_atomic(&resolve_sessions_path(state_dir), &bytes)
    }

    /// 标记「有未保存的变更」。
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 全部会话，按「新 → 旧」排列。
    pub fn sessions(&self) -> &[ResolveSession] {
        &self.sessions
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// 当前打开的会话（`None` = 新建解析空态）。
    pub fn active(&self) -> Option<&ResolveSession> {
        self.active
            .as_deref()
            .and_then(|id| self.sessions.iter().find(|session| session.id == id))
    }

    pub fn active_mut(&mut self) -> Option<&mut ResolveSession> {
        let id = self.active.clone()?;
        self.get_mut(&id)
    }

    pub fn active_id(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// 切换到已存在的会话；id 不存在返回 `false` 且保持原状态。
    pub fn set_active(&mut self, id: &str) -> bool {
        if self.get(id).is_none() {
            return false;
        }
        self.active = Some(id.to_string());
        true
    }

    /// 回到「新建解析」空态（不删除任何历史）。
    pub fn clear_active(&mut self) {
        self.active = None;
    }

    // -----------------------------------------------------------------------
    // 历史列表编辑（置顶 / 分组 / 删除）与同页面复用
    // -----------------------------------------------------------------------

    /// 删除一个会话；若删除的是当前会话，回到空态。未知 id 返回 `false`（不改脏）。
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
            return false;
        };
        self.sessions.remove(index);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        self.mark_dirty();
        true
    }

    /// 置顶 / 取消置顶；未知 id 返回 `false`。成功的修改标记待落盘。
    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> bool {
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return false;
        };
        session.pinned = pinned;
        self.mark_dirty();
        true
    }

    /// 设置分组（`None`/空白 = 未分组）；未知 id 返回 `false`。成功的修改标记待落盘。
    pub fn set_group(&mut self, id: &str, group: Option<&str>) -> bool {
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return false;
        };
        session.group = group.and_then(normalize_session_group);
        self.mark_dirty();
        true
    }

    /// 重命名分组：把 `old` 组下的全部会话移动到 `new` 组名；`new` 为空白 = 取消分组。
    ///
    /// - 返回被移动的会话数量（`0` 表示没有该分组或新旧同名，不产生变更）；
    /// - 新组名与现有分组同名时等价于「合并分组」（`groups()` 去重）；
    /// - 有变更才 `mark_dirty()`。
    pub fn rename_group(&mut self, old: &str, new: &str) -> usize {
        let Some(old_group) = normalize_session_group(old) else {
            return 0;
        };
        let new_group = normalize_session_group(new);
        if new_group.as_deref() == Some(old_group.as_str()) {
            return 0;
        }
        let mut moved = 0;
        for session in &mut self.sessions {
            if session.group.as_deref() == Some(old_group.as_str()) {
                session.group = new_group.clone();
                moved += 1;
            }
        }
        if moved > 0 {
            self.mark_dirty();
        }
        moved
    }

    /// 全部自定义分组（去重、按名称升序；未分组不在其中）。
    pub fn groups(&self) -> Vec<String> {
        let mut groups: BTreeSet<&str> = BTreeSet::new();
        for session in &self.sessions {
            if let Some(group) = session.group.as_deref() {
                groups.insert(group);
            }
        }
        groups.into_iter().map(str::to_owned).collect()
    }

    /// 某个分组的会话数量（`None` = 未分组）。
    pub fn group_count(&self, group: Option<&str>) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.group.as_deref() == group)
            .count()
    }

    /// 列表排序（历史视图的唯一排序规则）：置顶优先，其后按更新时间降序，
    /// 再按创建时间降序（同毫秒时保持稳定）。
    pub fn sorted_sessions(&self) -> Vec<&ResolveSession> {
        let mut sorted: Vec<&ResolveSession> = self.sessions.iter().collect();
        sorted.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| right.updated_ms.cmp(&left.updated_ms))
                .then_with(|| right.created_ms.cmp(&left.created_ms))
        });
        sorted
    }

    /// 按「规范化页面 URL」找最近更新的历史会话（同页面复用去重键）。
    ///
    /// `url` 可以是原始 URL（内部再规范化一次）；无法规范化时返回 `None`。
    pub fn find_by_source_url(&self, url: &str) -> Option<&ResolveSession> {
        let target = normalize_session_url(url)?;
        self.sessions
            .iter()
            .filter(|session| {
                normalize_session_url(&session.panel.source_url).as_deref() == Some(target.as_str())
            })
            .max_by_key(|session| session.updated_ms)
    }

    /// 同页面复用：返回可复用的会话 id（不创建新会话）。
    ///
    /// 只做查找，不改 active/消息；视图层据此决定「切换 / 原地重新解析」。
    pub fn find_id_by_source_url(&self, url: &str) -> Option<String> {
        self.find_by_source_url(url)
            .map(|session| session.id.clone())
    }

    /// 新建会话：id 形如 `rs_000001`，插到最前并设为当前会话。
    pub fn create_session(&mut self, origin: ResolveOrigin, now_ms: u64) -> String {
        let seq = self.next_seq.max(1);
        self.next_seq = seq.saturating_add(1);
        let id = format!("rs_{seq:06}");
        self.sessions
            .insert(0, ResolveSession::new(id.clone(), origin, now_ms));
        self.active = Some(id.clone());
        self.mark_dirty();
        id
    }

    pub fn get(&self, id: &str) -> Option<&ResolveSession> {
        self.sessions.iter().find(|session| session.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ResolveSession> {
        self.sessions.iter_mut().find(|session| session.id == id)
    }

    /// 按解析请求代号找会话（0 不是有效代号，返回 `None`）。
    pub fn find_mut_by_parse_request(&mut self, request_id: u64) -> Option<&mut ResolveSession> {
        if request_id == 0 {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|session| session.panel.request_id == request_id)
    }

    /// 按普通聊天/汇报流式请求 id 找会话。
    pub fn find_mut_by_chat_request(&mut self, request_id: u64) -> Option<&mut ResolveSession> {
        if request_id == 0 {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|session| session.chat_request == Some(request_id))
    }

    /// 按标题生成请求 id 找会话。
    pub fn find_mut_by_title_request(&mut self, request_id: u64) -> Option<&mut ResolveSession> {
        if request_id == 0 {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|session| session.title_request == Some(request_id))
    }

    /// 按工作流解析请求 id 找会话。
    pub fn find_mut_by_agent_request(&mut self, request_id: u64) -> Option<&mut ResolveSession> {
        if request_id == 0 {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|session| session.agent_request == Some(request_id))
    }

    /// 按工作流汇报请求 id 找会话。
    pub fn find_mut_by_agent_summary_request(
        &mut self,
        request_id: u64,
    ) -> Option<&mut ResolveSession> {
        if request_id == 0 {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|session| session.agent_summary_request == Some(request_id))
    }
}

// ---------------------------------------------------------------------------
// 纯函数辅助
// ---------------------------------------------------------------------------

/// 从 `rs_000123` 解析出序号（用于加载时延续 `next_seq`，防止 id 重复）。
fn session_seq(id: &str) -> Option<u64> {
    id.strip_prefix("rs_")?.parse::<u64>().ok()
}

/// 把分组名归一化：去首尾空白，空字符串视为「未分组」。
pub fn normalize_session_group(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 同一页面复用的去重键：规范化 URL。
///
/// 规则（设计契约 §2.2）：
/// - 仅 `http` / `https`（其它协议或非法 URL 返回 `None`，不参与去重）；
/// - host 小写（`url` crate 解析时已统一），去掉默认端口（http 80 / https 443）；
/// - 路径保留，空路径视为 `/`；
/// - **保留 query**（不同 query 可能是不同资源页），去掉 fragment；
/// - 规范化结果幂等（`normalize(normalize(x)) == normalize(x)`）。
pub fn normalize_session_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parsed = url::Url::parse(trimmed).ok()?;
    let default_port = match parsed.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => return None,
    };
    // 显式写出的默认端口归一化为「无端口」；`url` 解析通常已处理，这里再保险一次。
    if parsed.port() == default_port {
        let _ = parsed.set_port(None);
    }
    parsed.set_fragment(None);
    if parsed.path().is_empty() {
        parsed.set_path("/");
    }
    Some(parsed.to_string())
}

/// 确定性兜底标题（[`ResolveSession::fallback_title`] 的纯函数实现）。
fn fallback_title_for(panel: &AiPanelState) -> String {
    let source_url = panel.source_url.trim();
    if !source_url.is_empty() {
        let host = url::Url::parse(source_url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned))
            .filter(|host| !host.is_empty());
        let base = host.unwrap_or_else(|| source_url.to_owned());
        // 后缀必须与解析状态一致（U-11）：解析中资源必然为空，不能写成「解析失败」。
        let suffix = match panel.status {
            AiParseStatus::Idle => " · 待解析".to_owned(),
            AiParseStatus::Parsing => " · 解析中".to_owned(),
            AiParseStatus::Failed => " · 解析失败".to_owned(),
            AiParseStatus::Ready if panel.resources.is_empty() => " · 无资源".to_owned(),
            AiParseStatus::Ready => format!(" · {} 个资源", panel.resources.len()),
        };
        return clip_with_suffix(&base, &suffix, FALLBACK_TITLE_MAX_CHARS);
    }
    if let Some(message) = panel
        .messages
        .iter()
        .find(|message| message.role == AiRole::User)
    {
        let text = collapse_whitespace(message.text.trim());
        if !text.is_empty() {
            return truncate_chars(&text, MESSAGE_TITLE_MAX_CHARS);
        }
    }
    "新解析".to_owned()
}

/// 把 `base + suffix` 控制在 `max_chars` 个字符内；`base` 过长时以 `…` 结尾。
fn clip_with_suffix(base: &str, suffix: &str, max_chars: usize) -> String {
    let suffix_len = suffix.chars().count();
    if suffix_len >= max_chars {
        return truncate_chars(suffix, max_chars);
    }
    let base_max = max_chars - suffix_len;
    let base = collapse_whitespace(base);
    if base.chars().count() <= base_max {
        return format!("{base}{suffix}");
    }
    let mut head: String = base.chars().take(base_max.saturating_sub(1)).collect();
    head.push('…');
    format!("{head}{suffix}")
}

/// 截断到最多 `max_chars` 个字符（按字符，不切断中文/emoji；超出以 `…` 结尾）。
///
/// 与 `ai_resolve.rs` 的同名工具保持同样的「上限包含省略号」约定。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// 压缩空白：连续空白（含换行/制表符）合并成一个空格。
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 清洗 LLM 生成的标题；清洗后为空返回 `None`。
fn sanitize_generated_title(raw: &str) -> Option<String> {
    // 只取第一行：模型经常在标题后补一段解释。
    let mut text = raw.lines().next().unwrap_or("").trim();
    // 前缀（markdown/列表/编号）与包裹标记（引号/书名号）可能交替出现：
    // `# 「标题」`、`1. **标题**`、`- 【标题】`。剥到稳定为止。
    loop {
        let before = text;
        text = strip_title_prefix(text);
        text = strip_title_wrapping(text);
        if text == before {
            break;
        }
    }
    let text = collapse_whitespace(text);
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(&text, GENERATED_TITLE_MAX_CHARS))
}

/// 剥掉标题开头的前缀：markdown 标题/引用、列表符号、有序编号。
fn strip_title_prefix(text: &str) -> &str {
    let mut rest = text.trim_start();
    loop {
        let before = rest;
        // `#`/`>` 允许紧跟文字（`#标题`），与列表符号分开处理。
        rest = rest.trim_start_matches(['#', '>']).trim_start();
        // 列表符号只在后面是空白（或文本结束）时才算列表项，避免吃掉负号/数学符号。
        if let Some(stripped) = strip_bullet(rest) {
            rest = stripped.trim_start();
        }
        // 有序编号：`1.` / `1、` / `1)` / `1．`（最多 3 位数字 + 分隔符）。
        rest = strip_ordered_number(rest).trim_start();
        if rest == before {
            break;
        }
    }
    rest
}

fn strip_bullet(text: &str) -> Option<&str> {
    let first = text.chars().next()?;
    if !matches!(first, '-' | '*' | '•' | '·') {
        return None;
    }
    let stripped = &text[first.len_utf8()..];
    let list_item = stripped.is_empty() || stripped.chars().next().is_some_and(char::is_whitespace);
    list_item.then_some(stripped)
}

fn strip_ordered_number(text: &str) -> &str {
    let digits_end = text
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if digits_end == 0 || digits_end > 3 {
        return text;
    }
    let rest = &text[digits_end..];
    let after = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix('、'))
        .or_else(|| rest.strip_prefix(')'))
        .or_else(|| rest.strip_prefix('．'));
    match after {
        Some(after) => after.trim_start(),
        None => text,
    }
}

/// 可对称剥掉的包裹符号（开=闭）。
const SYMMETRIC_MARKS: [char; 5] = ['"', '\'', '`', '*', '~'];
/// 成对被包裹的符号（开, 闭）。
const PAIRED_MARKS: [(char, char); 8] = [
    ('“', '”'),
    ('‘', '’'),
    ('「', '」'),
    ('『', '』'),
    ('《', '》'),
    ('【', '】'),
    ('(', ')'),
    ('（', '）'),
];
/// 只出现在开头的引号类噪声（模型常漏掉另一半）。
const OPEN_MARKS: [char; 11] = ['"', '\'', '`', '*', '~', '“', '‘', '「', '『', '《', '【'];
/// 只出现在结尾的引号类噪声。
const CLOSE_MARKS: [char; 11] = ['"', '\'', '`', '*', '~', '”', '’', '」', '』', '》', '】'];

/// 剥掉包裹标题的引号/书名号/markdown 强调符号（成对优先，单边噪声也清）。
fn strip_title_wrapping(text: &str) -> &str {
    let mut text = text.trim();
    loop {
        let before = text;
        if text.chars().count() >= 2 {
            let first = text.chars().next();
            let last = text.chars().next_back();
            if let (Some(first), Some(last)) = (first, last) {
                let symmetric = first == last && SYMMETRIC_MARKS.contains(&first);
                let paired = PAIRED_MARKS
                    .iter()
                    .any(|(open, close)| *open == first && *close == last);
                if symmetric || paired {
                    text = text[first.len_utf8()..text.len() - last.len_utf8()].trim();
                    continue;
                }
            }
        }
        if let Some(first) = text.chars().next() {
            if OPEN_MARKS.contains(&first) {
                text = text[first.len_utf8()..].trim_start();
                continue;
            }
        }
        if let Some(last) = text.chars().next_back() {
            if CLOSE_MARKS.contains(&last) {
                text = text[..text.len() - last.len_utf8()].trim_end();
                continue;
            }
        }
        if text == before {
            break;
        }
    }
    text
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::agent::{AgentIntent, AgentPlan};
    use crate::model::{now_ms, ParseCredential};
    use serde_json::{json, Value};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 测试临时目录的根：优先系统临时目录；不可写（例如受限沙箱）时退到
    /// `target/<profile>/test-tmp/`——它随 `target/` 一起被 gitignore，且构建/测试
    /// 一定有写权限。
    fn test_root() -> PathBuf {
        let mut candidates: Vec<PathBuf> = Vec::new();
        candidates.push(std::env::temp_dir().join("rdm-gui-resolve-session-tests"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(profile_dir) = exe.parent().and_then(Path::parent) {
                candidates.push(profile_dir.join("test-tmp").join("resolve-session"));
            }
        }
        for candidate in candidates {
            if std::fs::create_dir_all(&candidate).is_ok() {
                return candidate;
            }
        }
        panic!("没有可写的测试临时目录（系统临时目录与 target/ 均不可写）");
    }

    /// 测试用 state 目录（唯一子目录，Drop 时清理）。
    struct TempStateDir {
        path: PathBuf,
    }

    impl TempStateDir {
        fn new() -> Self {
            let unique = format!(
                "rdm-gui-resolve-session-{}-{}-{}",
                std::process::id(),
                now_ms(),
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let path = test_root().join(unique);
            std::fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempStateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn resource(name: &str, kind: &str) -> ParsedResource {
        ParsedResource {
            url: format!("https://example.com/{name}"),
            name: name.to_string(),
            kind: kind.to_string(),
            size_bytes: Some(1024),
            content_type: Some("application/octet-stream".to_string()),
            note: None,
        }
    }

    fn card(name: &str, kind: &str, selected: bool) -> AiResourceCard {
        AiResourceCard {
            resource: resource(name, kind),
            selected,
        }
    }

    fn user_message(text: &str, created_ms: u64) -> AiChatMessage {
        AiChatMessage {
            role: AiRole::User,
            text: text.to_string(),
            created_ms,
        }
    }

    fn system_message(text: &str, created_ms: u64) -> AiChatMessage {
        AiChatMessage {
            role: AiRole::System,
            text: text.to_string(),
            created_ms,
        }
    }

    fn read_file_json(dir: &Path) -> Value {
        let text = std::fs::read_to_string(resolve_sessions_path(dir)).expect("读取持久化文件");
        serde_json::from_str(&text).expect("持久化 JSON 可解析")
    }

    // 1 ---------------------------------------------------------------------
    #[test]
    fn create_session_ids_increment_and_active_is_newest() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert_eq!(store.next_seq, 1);
        assert!(store.active().is_none());
        assert!(!store.is_dirty());

        let first = store.create_session(ResolveOrigin::Gui, 1_000);
        let second = store.create_session(ResolveOrigin::Extension, 2_000);
        assert_eq!(first, "rs_000001");
        assert_eq!(second, "rs_000002");
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        assert_eq!(store.active_id(), Some(second.as_str()));
        assert_eq!(store.active().map(|s| s.id.as_str()), Some(second.as_str()));
        // 新 → 旧
        assert_eq!(store.sessions()[0].id, second);
        assert_eq!(store.sessions()[1].id, first);
        assert_eq!(store.sessions()[0].origin, ResolveOrigin::Extension);
        assert_eq!(store.sessions()[0].created_ms, 2_000);
        assert_eq!(store.sessions()[0].updated_ms, 2_000);
        assert_eq!(store.sessions()[0].title, "新解析");
        assert_eq!(store.sessions()[0].title_source, TitleSource::Fallback);
        assert!(store.is_dirty());

        // 切换/清空 active
        assert!(store.set_active(&first));
        assert_eq!(store.active_id(), Some(first.as_str()));
        assert!(store.active_mut().is_some());
        assert!(!store.set_active("rs_999999"));
        assert_eq!(store.active_id(), Some(first.as_str()));
        store.clear_active();
        assert!(store.active().is_none());
        assert!(store.active_mut().is_none());
        assert!(store.active_id().is_none());
    }

    // 2 ---------------------------------------------------------------------
    #[test]
    fn persisted_sessions_roundtrip_with_usage_and_next_seq() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());

        let gui_id = store.create_session(ResolveOrigin::Gui, 1_000);
        {
            let session = store.get_mut(&gui_id).unwrap();
            session.touch(2_000);
            session.panel.source_url = "https://example.com/page".to_string();
            session.panel.status = AiParseStatus::Ready;
            session.panel.notes = vec!["笔记一".to_string(), "笔记二".to_string()];
            session.panel.ai_used = true;
            session.panel.cache_hit = true;
            session.panel.model = Some("deepseek-chat".to_string());
            session.panel.usage = TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 20,
                total_tokens: 120,
                calls: 1,
                estimated: false,
            };
            session.panel.error = None;
            session.panel.push_message(AiRole::User, "帮我下载这个");
            session
                .panel
                .push_message(AiRole::Assistant, "找到 2 个资源");
            session.panel.resources = vec![
                card("a.iso", "archive", true),
                card("b.iso", "archive", false),
            ];
            session.title = session.fallback_title();
            session.title_source = TitleSource::Fallback;
        }

        let ext_id = store.create_session(ResolveOrigin::Extension, 3_000);
        {
            let session = store.get_mut(&ext_id).unwrap();
            session.panel.source_url = "https://extension.example/file".to_string();
            session.panel.status = AiParseStatus::Failed;
            session.panel.error = Some("无法访问页面".to_string());
            session.panel.push_message(AiRole::System, "解析失败");
            assert!(session.apply_generated_title("扩展会话标题", 4_000));
        }

        assert!(store.save_if_dirty(dir.path()).unwrap());
        assert!(!store.is_dirty());
        assert_eq!(store.next_seq, 3);

        // 文件根对象形状：version / next_seq / sessions
        let json = read_file_json(dir.path());
        assert_eq!(json["version"], json!(RESOLVE_SESSIONS_VERSION));
        assert_eq!(json["next_seq"], json!(3));
        let sessions = json["sessions"].as_array().expect("sessions 是数组");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["id"], json!(ext_id));
        assert_eq!(sessions[0]["title"], json!("扩展会话标题"));
        assert_eq!(sessions[0]["title_source"], json!("llm"));
        assert_eq!(sessions[0]["origin"], json!("extension"));
        assert_eq!(sessions[0]["status"], json!("failed"));
        assert_eq!(sessions[1]["status"], json!("ready"));
        assert_eq!(sessions[1]["origin"], json!("gui"));
        assert_eq!(sessions[1]["title_source"], json!("fallback"));
        assert_eq!(sessions[1]["resources"][0]["selected"], json!(true));
        assert_eq!(sessions[1]["resources"][1]["selected"], json!(false));
        assert_eq!(sessions[1]["messages"][0]["role"], json!("user"));
        assert_eq!(sessions[1]["messages"][1]["role"], json!("assistant"));

        // 加载后逐字段一致（用 to_persisted 做全量比较，避免漏字段）
        let loaded = ResolveStore::load(dir.path());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.next_seq, 3);
        assert_eq!(loaded.sessions()[0].id, ext_id);
        assert_eq!(loaded.sessions()[1].id, gui_id);
        assert_eq!(loaded.active_id(), Some(ext_id.as_str())); // 恢复最新会话
        assert_eq!(
            loaded.sessions()[0].to_persisted(),
            store.sessions()[0].to_persisted()
        );
        assert_eq!(
            loaded.sessions()[1].to_persisted(),
            store.sessions()[1].to_persisted()
        );
        let restored = loaded.get(&gui_id).unwrap();
        assert_eq!(restored.title, "example.com · 2 个资源");
        assert_eq!(restored.created_ms, 1_000);
        assert_eq!(restored.updated_ms, 2_000);
        assert_eq!(restored.origin, ResolveOrigin::Gui);
        assert_eq!(restored.panel.usage.total_tokens, 120);
        assert_eq!(restored.panel.usage.calls, 1);
        assert_eq!(restored.panel.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(restored.panel.notes, vec!["笔记一", "笔记二"]);
        assert!(restored.panel.ai_used);
        assert!(restored.panel.cache_hit);
        assert_eq!(restored.panel.messages.len(), 2);
        assert_eq!(restored.panel.messages[0].role, AiRole::User);
        assert_eq!(restored.panel.messages[1].role, AiRole::Assistant);
        assert!(!restored.panel.resources[1].selected);
        let failed = loaded.get(&ext_id).unwrap();
        assert_eq!(
            failed.panel.error.as_deref(),
            Some("无法访问页面"),
            "失败原因要保留"
        );

        // next_seq 延续：新会话不覆盖旧 id
        let mut loaded = loaded;
        let next = loaded.create_session(ResolveOrigin::Agent, 5_000);
        assert_eq!(next, "rs_000003");
    }

    // 3 ---------------------------------------------------------------------
    #[test]
    fn memory_only_fields_are_never_persisted() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());
        let id = store.create_session(ResolveOrigin::Agent, 10);
        {
            let session = store.get_mut(&id).unwrap();
            session.plan = Some(AgentPlan::new(
                AgentIntent::Resolve,
                "https://example.com/x",
                "下载它",
                11,
            ));
            session.title_request = Some(101);
            session.chat_request = Some(102);
            session.agent_request = Some(103);
            session.agent_summary_request = Some(104);
            session.panel.request_id = 105;
            session.panel.chat_pending = true;
            session.panel.streaming = true;
            session.panel.credential = Some(ParseCredential {
                host: "example.com".to_string(),
                cookie: Some("SESSION=secret-value".to_string()),
                referer: Some("https://example.com/".to_string()),
                user_agent: Some("RDM-Test-UA".to_string()),
            });
        }
        assert!(store.save_if_dirty(dir.path()).unwrap());

        let text = std::fs::read_to_string(resolve_sessions_path(dir.path())).unwrap();
        for forbidden in [
            "SESSION=secret-value",
            "RDM-Test-UA",
            "credential",
            "cookie",
            "request_id",
            "\"plan\"",
            "chat_pending",
            "streaming",
        ] {
            assert!(!text.contains(forbidden), "持久化文件不能包含 {forbidden}");
        }

        let loaded = ResolveStore::load(dir.path());
        let session = loaded.get(&id).unwrap();
        assert!(session.plan.is_none());
        assert!(session.title_request.is_none());
        assert!(session.chat_request.is_none());
        assert!(session.agent_request.is_none());
        assert!(session.agent_summary_request.is_none());
        assert!(session.panel.credential.is_none());
        assert_eq!(session.panel.request_id, 0);
        assert!(!session.panel.chat_pending);
        assert!(!session.panel.streaming);
        assert!(session.panel.last_plan_summary.is_none());
    }

    // 4 ---------------------------------------------------------------------
    #[test]
    fn corrupt_file_loads_as_empty_store_without_panic() {
        let dir = TempStateDir::new();
        let path = resolve_sessions_path(dir.path());
        std::fs::write(&path, b"{ this is not json").unwrap();
        let store = ResolveStore::load(dir.path());
        assert!(store.is_empty());
        assert_eq!(store.next_seq, 1);
        assert!(!store.is_dirty());
        assert!(store.active().is_none());

        // 非 UTF-8 字节同样只降级为空 store
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x81]).unwrap();
        assert!(ResolveStore::load(dir.path()).is_empty());

        // 空文件 / 合法 JSON 但不是对象
        std::fs::write(&path, b"").unwrap();
        assert!(ResolveStore::load(dir.path()).is_empty());
        std::fs::write(&path, b"[]").unwrap();
        assert!(ResolveStore::load(dir.path()).is_empty());
    }

    #[test]
    fn old_json_missing_fields_uses_defaults() {
        let dir = TempStateDir::new();
        let path = resolve_sessions_path(dir.path());
        // 旧文件：缺 version/next_seq/各类新字段；消息缺 role/created_ms。
        std::fs::write(
            &path,
            r#"{"sessions":[{"id":"rs_000009","title":"旧会话","status":"ready","messages":[{"text":"旧消息"}]}]}"#,
        )
        .unwrap();
        let mut store = ResolveStore::load(dir.path());
        assert_eq!(store.len(), 1);
        assert_eq!(store.next_seq, 10); // 从最大 id 序号延续
        let session = store.get("rs_000009").unwrap();
        assert_eq!(session.title, "旧会话");
        assert_eq!(session.title_source, TitleSource::Fallback);
        assert_eq!(session.origin, ResolveOrigin::Gui);
        assert_eq!(session.created_ms, 0);
        assert_eq!(session.updated_ms, 0);
        assert_eq!(session.panel.status, AiParseStatus::Ready);
        assert_eq!(session.panel.messages.len(), 1);
        assert_eq!(session.panel.messages[0].role, AiRole::System);
        assert_eq!(session.panel.messages[0].text, "旧消息");
        assert!(session.panel.notes.is_empty());
        assert!(session.panel.error.is_none());
        assert_eq!(session.panel.usage, TokenUsage::default());

        // next_seq 继续递增
        let next = store.create_session(ResolveOrigin::Gui, 1);
        assert_eq!(next, "rs_000010");
    }

    // 5 ---------------------------------------------------------------------
    #[test]
    fn interrupted_parsing_session_recovers_as_failed() {
        let dir = TempStateDir::new();
        let path = resolve_sessions_path(dir.path());
        let messages: Vec<Value> = (0..250)
            .map(|index| json!({"role": "user", "text": format!("m{index}"), "created_ms": index}))
            .collect();
        let doc = json!({
            "version": 1,
            "next_seq": 2,
            "sessions": [{
                "id": "rs_000001",
                "title": "中断的会话",
                "status": "parsing",
                "source_url": "https://example.com/half",
                "messages": messages,
            }]
        });
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();

        let store = ResolveStore::load(dir.path());
        let session = store.get("rs_000001").expect("会话应被加载");
        assert_eq!(session.panel.status, AiParseStatus::Failed);
        assert_eq!(
            session.panel.error.as_deref(),
            Some(INTERRUPTED_PARSE_ERROR)
        );
        assert!(session.has_parse_result());
        // 消息超上限：只保留末尾 200 条（丢最旧）
        assert_eq!(session.panel.messages.len(), AI_CHAT_HISTORY_LIMIT);
        assert_eq!(session.panel.messages.first().unwrap().text, "m50");
        assert_eq!(session.panel.messages.last().unwrap().text, "m249");
    }

    #[test]
    fn save_truncates_messages_to_history_limit() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());
        let id = store.create_session(ResolveOrigin::Gui, 1);
        {
            let session = store.get_mut(&id).unwrap();
            session.panel.messages = (0..250)
                .map(|index| user_message(&format!("m{index}"), index))
                .collect();
        }
        assert!(store.save_if_dirty(dir.path()).unwrap());

        let json = read_file_json(dir.path());
        let saved = json["sessions"][0]["messages"].as_array().unwrap();
        assert_eq!(saved.len(), AI_CHAT_HISTORY_LIMIT);
        assert_eq!(saved.first().unwrap()["text"], json!("m50"));
        assert_eq!(saved.last().unwrap()["text"], json!("m249"));
    }

    // 6 ---------------------------------------------------------------------
    #[test]
    fn generated_title_is_cleaned_before_apply() {
        let mut session = ResolveSession::new("rs_000001".to_string(), ResolveOrigin::Gui, 1);
        assert!(session.apply_generated_title("# 「下载页面标题」\n这是解释", 42));
        assert_eq!(session.title, "下载页面标题");
        assert_eq!(session.title_source, TitleSource::Llm);
        assert_eq!(session.updated_ms, 42);

        assert!(session.apply_generated_title("  1. 音乐合集  ", 43));
        assert_eq!(session.title, "音乐合集");

        assert!(session.apply_generated_title("- **视频合集**", 44));
        assert_eq!(session.title, "视频合集");

        assert!(session.apply_generated_title("\"第一行标题\"\n第二行", 45));
        assert_eq!(session.title, "第一行标题");

        assert!(session.apply_generated_title("2、压缩包合集", 46));
        assert_eq!(session.title, "压缩包合集");

        // 空/只有噪声 → false 且不改动已有标题
        let before_title = session.title.clone();
        let before_source = session.title_source;
        let before_updated = session.updated_ms;
        for empty in ["", "   ", "\n\n", "「」", "## ", "- "] {
            assert!(
                !session.apply_generated_title(empty, 99),
                "空标题 {empty:?} 不应生效"
            );
        }
        assert_eq!(session.title, before_title);
        assert_eq!(session.title_source, before_source);
        assert_eq!(session.updated_ms, before_updated);

        // 超长截断到上限（含省略号）
        assert!(session.apply_generated_title(&"长".repeat(80), 47));
        assert_eq!(
            session.title.chars().count(),
            GENERATED_TITLE_MAX_CHARS,
            "标题: {}",
            session.title
        );
        assert!(session.title.ends_with('…'));
    }

    // 7 ---------------------------------------------------------------------
    #[test]
    fn fallback_title_covers_url_resources_and_messages() {
        let mut session = ResolveSession::new("rs_000001".to_string(), ResolveOrigin::Gui, 1);
        assert_eq!(session.fallback_title(), "新解析");

        // 有 URL + 资源 → host · N 个资源
        session.panel.source_url = "https://example.com/files/page".to_string();
        session.panel.status = AiParseStatus::Ready;
        session.panel.resources = vec![
            card("a.iso", "archive", true),
            card("b.iso", "archive", true),
        ];
        assert_eq!(session.fallback_title(), "example.com · 2 个资源");

        // 解析失败 → host · 解析失败
        session.panel.status = AiParseStatus::Failed;
        assert_eq!(session.fallback_title(), "example.com · 解析失败");

        // 解析中（资源必为空）→ host · 解析中，不能误报「解析失败」（U-11）
        session.panel.status = AiParseStatus::Parsing;
        assert_eq!(session.fallback_title(), "example.com · 解析中");

        // 成功但没有可下载资源 → host · 无资源（不是「解析失败」）
        session.panel.status = AiParseStatus::Ready;
        session.panel.resources.clear();
        assert_eq!(session.fallback_title(), "example.com · 无资源");

        // 无 URL → 第一条用户消息前 24 字符（跳过系统消息）
        session.panel.source_url.clear();
        session
            .panel
            .messages
            .push(system_message("忽略系统消息", 1));
        session
            .panel
            .messages
            .push(user_message("帮我下载这个视频谢谢", 2));
        assert_eq!(session.fallback_title(), "帮我下载这个视频谢谢");

        // 无消息 → 新解析
        session.panel.messages.clear();
        assert_eq!(session.fallback_title(), "新解析");

        // 超长消息截断到 24 字符以内并以省略号结尾
        session
            .panel
            .messages
            .push(user_message(&"长".repeat(80), 3));
        let title = session.fallback_title();
        assert!(title.chars().count() <= MESSAGE_TITLE_MAX_CHARS, "{title}");
        assert!(title.ends_with('…'), "{title}");

        // 超长 host + 资源计数：总长仍 ≤ 40，且保留后缀
        session.panel.messages.clear();
        session.panel.source_url = format!("https://{}.example.com/x", "a".repeat(80));
        session.panel.status = AiParseStatus::Ready;
        session.panel.resources = vec![
            card("a", "archive", true),
            card("b", "archive", true),
            card("c", "archive", true),
        ];
        let title = session.fallback_title();
        assert!(title.chars().count() <= FALLBACK_TITLE_MAX_CHARS, "{title}");
        assert!(title.ends_with(" · 3 个资源"), "{title}");
        assert!(title.contains('…'), "{title}");

        // 非法 URL：用 URL 本身兜底，不 panic
        session.panel.source_url = "not a url".to_string();
        session.panel.resources.clear();
        session.panel.status = AiParseStatus::Failed;
        assert_eq!(session.fallback_title(), "not a url · 解析失败");
        // 纯空白 URL 视为没有 URL
        session.panel.source_url = "   ".to_string();
        assert_eq!(session.fallback_title(), "新解析");
    }

    // 8 ---------------------------------------------------------------------
    #[test]
    fn find_mut_by_request_id_matches_only_the_owning_session() {
        let mut store = ResolveStore::default();
        let first = store.create_session(ResolveOrigin::Gui, 1);
        let second = store.create_session(ResolveOrigin::Agent, 2);
        {
            let session = store.get_mut(&first).unwrap();
            session.panel.request_id = 11;
            session.chat_request = Some(12);
            session.title_request = Some(13);
        }
        {
            let session = store.get_mut(&second).unwrap();
            session.panel.request_id = 21;
            session.chat_request = Some(22);
            session.agent_request = Some(23);
            session.agent_summary_request = Some(24);
        }

        assert_eq!(
            store.find_mut_by_parse_request(11).map(|s| s.id.as_str()),
            Some(first.as_str())
        );
        assert_eq!(
            store.find_mut_by_parse_request(21).map(|s| s.id.as_str()),
            Some(second.as_str())
        );
        assert_eq!(
            store.find_mut_by_chat_request(22).map(|s| s.id.as_str()),
            Some(second.as_str())
        );
        assert_eq!(
            store.find_mut_by_title_request(13).map(|s| s.id.as_str()),
            Some(first.as_str())
        );
        assert_eq!(
            store.find_mut_by_agent_request(23).map(|s| s.id.as_str()),
            Some(second.as_str())
        );
        assert_eq!(
            store
                .find_mut_by_agent_summary_request(24)
                .map(|s| s.id.as_str()),
            Some(second.as_str())
        );

        // 未知代号 / 0（表示没有请求）→ None
        for unknown in [99_u64, 0] {
            assert!(store.find_mut_by_parse_request(unknown).is_none());
            assert!(store.find_mut_by_chat_request(unknown).is_none());
            assert!(store.find_mut_by_title_request(unknown).is_none());
            assert!(store.find_mut_by_agent_request(unknown).is_none());
            assert!(store.find_mut_by_agent_summary_request(unknown).is_none());
        }
        // 返回的 &mut 可写回会话
        store.find_mut_by_agent_request(23).unwrap().title = "工作流会话".to_string();
        assert_eq!(store.get(&second).unwrap().title, "工作流会话");
    }

    // 9 ---------------------------------------------------------------------
    #[test]
    fn save_if_dirty_only_writes_after_mark_dirty() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());
        assert!(!store.is_dirty());
        assert!(!store.save_if_dirty(dir.path()).unwrap());
        assert!(
            !resolve_sessions_path(dir.path()).exists(),
            "没有变化时不应创建文件"
        );

        store.create_session(ResolveOrigin::Gui, 1);
        assert!(store.is_dirty());
        assert!(store.save_if_dirty(dir.path()).unwrap());
        assert!(!store.is_dirty());
        assert!(resolve_sessions_path(dir.path()).exists());
        assert!(
            !store.save_if_dirty(dir.path()).unwrap(),
            "保存后应清空脏标记"
        );

        // 直接 mark_dirty 也会写一次
        store.mark_dirty();
        assert!(store.is_dirty());
        assert!(store.save_if_dirty(dir.path()).unwrap());
        assert!(!store.is_dirty());

        // save_now 无条件写（且不要求 dirty）
        store.save_now(dir.path()).unwrap();
        assert!(!store.is_dirty());
    }

    // 10 --------------------------------------------------------------------
    #[test]
    fn title_source_starts_fallback_and_turns_llm_after_apply() {
        let mut store = ResolveStore::default();
        let id = store.create_session(ResolveOrigin::Gui, 1);
        {
            let session = store.get_mut(&id).unwrap();
            assert_eq!(session.title_source, TitleSource::Fallback);
            assert_eq!(session.title, "新解析");
            assert!(!session.has_parse_result());
            assert!(!session.needs_title());
            assert_eq!(session.status_label_zh(), "未解析");
            assert_eq!(session.resource_count(), 0);

            session.panel.status = AiParseStatus::Parsing;
            assert_eq!(session.status_label_zh(), "解析中…");
            assert!(!session.has_parse_result());

            session.panel.status = AiParseStatus::Ready;
            assert!(session.has_parse_result());
            assert!(session.needs_title());
            assert_eq!(session.status_label_zh(), "已解析");
            assert_eq!(session.resource_count(), 0);

            assert!(session.apply_generated_title("下载页标题", 2));
            assert_eq!(session.title_source, TitleSource::Llm);
            assert!(!session.needs_title());
        }

        // 失败也视为「有结果」，仍允许生成标题
        let failed_id = store.create_session(ResolveOrigin::Agent, 3);
        {
            let session = store.get_mut(&failed_id).unwrap();
            session.panel.status = AiParseStatus::Failed;
            assert!(session.has_parse_result());
            assert!(session.needs_title());
            assert_eq!(session.status_label_zh(), "解析失败");
        }

        // 来源文案
        assert_eq!(ResolveOrigin::Gui.label_zh(), "桌面端");
        assert_eq!(ResolveOrigin::Extension.label_zh(), "浏览器扩展");
        assert_eq!(ResolveOrigin::Agent.label_zh(), "Agent");
    }

    // 追加：标题 prompt 上下文有界；无上下文返回 None ---------------------------
    #[test]
    fn title_prompt_is_bounded_and_none_without_context() {
        let mut session = ResolveSession::new("rs_000001".to_string(), ResolveOrigin::Gui, 1);
        assert!(session.title_prompt().is_none());

        session.panel.source_url = "https://example.com/demo/page?x=1".to_string();
        let prompt = session
            .title_prompt()
            .expect("有来源 URL 就应该生成 prompt");
        assert!(prompt.contains("只输出一个 ≤20 字的会话标题"));
        assert!(prompt.contains("不要引号、不要编号、不要解释"));
        assert!(prompt.contains("https://example.com/demo/page?x=1"));

        session.panel.notes = (0..8).map(|index| format!("note-{index}")).collect();
        session.panel.resources = (0..10)
            .map(|index| card(&format!("res-{index}"), "video", true))
            .collect();
        session
            .panel
            .messages
            .push(user_message(&"请".repeat(300), 2));

        let prompt = session.title_prompt().unwrap();
        assert!(prompt.contains("note-4"));
        assert!(!prompt.contains("note-5"), "笔记只带前 5 条");
        assert!(prompt.contains("res-7"));
        assert!(!prompt.contains("res-8"), "资源只带前 8 个");
        let user_line = prompt
            .lines()
            .find(|line| line.starts_with("第一条用户消息："))
            .expect("prompt 应包含第一条用户消息");
        let user_prefix = "第一条用户消息：";
        assert!(
            user_line.chars().count() <= user_prefix.chars().count() + 200,
            "用户消息最多 200 字: {}",
            user_line.chars().count()
        );

        // 只有笔记也算有上下文
        let mut notes_only = ResolveSession::new("rs_000002".to_string(), ResolveOrigin::Gui, 1);
        notes_only.panel.notes = vec!["只有一个笔记".to_string()];
        assert!(notes_only.title_prompt().is_some());

        // 只有空白上下文 → None
        let mut blank = ResolveSession::new("rs_000003".to_string(), ResolveOrigin::Gui, 1);
        blank.panel.notes = vec!["   ".to_string()];
        blank.panel.messages.push(user_message("   ", 1));
        assert!(blank.title_prompt().is_none());
    }

    // 追加：路径 helper --------------------------------------------------------
    #[test]
    fn sessions_path_lives_under_state_dir() {
        let state_dir = Path::new("C:/state");
        assert_eq!(
            resolve_sessions_path(state_dir),
            state_dir.join(RESOLVE_SESSIONS_FILE)
        );
        assert!(resolve_sessions_path(state_dir).ends_with("resolve_sessions.json"));
    }

    // 追加：来源 → 下载来源 token（审计 F-05 的唯一映射） ---------------------
    #[test]
    fn origin_download_source_tokens_are_stable() {
        assert_eq!(ResolveOrigin::Gui.download_source(), "gui");
        assert_eq!(ResolveOrigin::Extension.download_source(), "extension");
        assert_eq!(ResolveOrigin::Agent.download_source(), "agent");
    }

    // 追加：解析 token 快照属于会话内存态，不得落盘（审计 F-02） --------------
    #[test]
    fn last_parse_usage_is_memory_only() {
        let dir = TempStateDir::new();
        let store = ResolveStore::load(dir.path());
        assert!(store.is_empty());

        let mut session = ResolveSession::new("rs_000001".to_owned(), ResolveOrigin::Agent, 1);
        session.panel.status = AiParseStatus::Ready;
        session.panel.source_url = "https://example.com/a".to_owned();
        session.panel.resources.push(card("a.mp4", "video", true));
        session.last_parse_usage = Some(ParseUsageSnapshot {
            request_id: 7,
            usage: TokenUsage::single(10, 5),
            model: Some("mock-model".to_owned()),
            cache_hit: false,
            cost_cny: Some(0.01),
        });
        assert!(session.last_parse_usage.is_some());

        let persisted = session.to_persisted();
        let json = serde_json::to_string(&persisted).expect("serialize");
        assert!(
            !json.contains("last_parse_usage"),
            "内存态 token 快照不能进持久化 JSON: {json}"
        );
        let restored = ResolveSession::from_persisted(persisted);
        assert!(
            restored.last_parse_usage.is_none(),
            "加载后的会话必须没有 token 快照（由本次运行重新产生）"
        );
    }

    // 追加：同页面复用的 URL 规范化（设计契约 §2.2） --------------------------
    #[test]
    fn normalize_session_url_dedupes_equivalent_page_urls() {
        // scheme/host 小写、fragment 丢弃、query 保留
        assert_eq!(
            normalize_session_url("HTTPS://Example.COM/Path?B=2#frag").as_deref(),
            Some("https://example.com/Path?B=2")
        );
        // 默认端口归一化
        assert_eq!(
            normalize_session_url("https://example.com:443/a").as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            normalize_session_url("http://example.com:80/a").as_deref(),
            Some("http://example.com/a")
        );
        // 非默认端口保留
        assert_eq!(
            normalize_session_url("http://example.com:8080/a").as_deref(),
            Some("http://example.com:8080/a")
        );
        // 空路径视为 `/`
        assert_eq!(
            normalize_session_url("https://example.com").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            normalize_session_url("https://example.com?x=1").as_deref(),
            Some("https://example.com/?x=1")
        );
        // query 保留且参与去重（不同 query = 不同页面）
        assert_eq!(
            normalize_session_url("https://example.com/page?x=1&y=2").as_deref(),
            Some("https://example.com/page?x=1&y=2")
        );
        assert_ne!(
            normalize_session_url("https://example.com/page?x=1"),
            normalize_session_url("https://example.com/page?x=2")
        );
        // 只有 fragment 不同的 URL 视为同一页面
        assert_eq!(
            normalize_session_url("https://example.com/page#a"),
            normalize_session_url("https://example.com/page#b")
        );
        // 非 http(s) / 非法 URL 不参与去重
        for raw in [
            "",
            "   ",
            "not a url",
            "ftp://example.com/a",
            "file:///C:/a",
            "mailto:a@example.com",
        ] {
            assert!(
                normalize_session_url(raw).is_none(),
                "{raw:?} 不应产生去重键"
            );
        }
        // 幂等
        let once = normalize_session_url("HTTPS://Example.COM/a?b=1#f").unwrap();
        assert_eq!(normalize_session_url(&once).as_deref(), Some(once.as_str()));
    }

    // 追加：置顶 / 分组 / 删除（编辑历史 + 脏标记 + active 清理） -------------
    #[test]
    fn pin_group_remove_edit_store_and_clear_active() {
        let mut store = ResolveStore::default();
        let first = store.create_session(ResolveOrigin::Gui, 1_000);
        let second = store.create_session(ResolveOrigin::Extension, 2_000);
        assert_eq!(store.active_id(), Some(second.as_str()));

        // 置顶
        assert!(store.set_pinned(&first, true));
        assert!(store.get(&first).unwrap().pinned);
        assert!(store.is_dirty());
        assert!(store.set_pinned(&first, false));
        assert!(!store.get(&first).unwrap().pinned);
        assert!(store.set_pinned(&second, true));

        // 分组（空白 = 未分组）
        assert!(store.set_group(&first, Some(" 番剧 ")));
        assert_eq!(store.get(&first).unwrap().group.as_deref(), Some("番剧"));
        assert!(store.set_group(&first, Some("   ")));
        assert_eq!(store.get(&first).unwrap().group, None);
        assert!(store.set_group(&first, Some("课程")));
        assert_eq!(store.groups(), vec!["课程".to_string()]);
        assert_eq!(store.group_count(Some("课程")), 1);
        assert_eq!(store.group_count(None), 1);

        // 未知 id：全部返回 false，且不产生副作用
        for unknown in ["rs_999999", ""] {
            assert!(!store.set_pinned(unknown, true));
            assert!(!store.set_group(unknown, Some("x")));
            assert!(!store.remove(unknown));
        }
        assert_eq!(store.len(), 2);

        // 删除非当前会话：active 保持不变
        assert!(store.remove(&first));
        assert_eq!(store.active_id(), Some(second.as_str()));
        assert_eq!(store.len(), 1);
        // 删除当前会话：回到空态
        assert!(store.remove(&second));
        assert!(store.active_id().is_none());
        assert!(store.is_empty());
        // 删除后已置顶/分组的索引不再命中
        assert!(store.groups().is_empty());
    }

    // 追加：列表排序 pinned 优先，其后 updated 降序 --------------------------
    #[test]
    fn sorted_sessions_put_pinned_first_then_most_recent() {
        let mut store = ResolveStore::default();
        let a = store.create_session(ResolveOrigin::Gui, 1_000);
        let b = store.create_session(ResolveOrigin::Gui, 2_000);
        let c = store.create_session(ResolveOrigin::Gui, 3_000);
        store.get_mut(&a).unwrap().touch(100);
        store.get_mut(&b).unwrap().touch(300);
        store.get_mut(&c).unwrap().touch(200);

        let ids: Vec<&str> = store
            .sorted_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, vec![b.as_str(), c.as_str(), a.as_str()]);

        // 置顶最旧的一条后仍排最前（pinned 优先于 updated）
        store.set_pinned(&a, true);
        let ids: Vec<&str> = store
            .sorted_sessions()
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, vec![a.as_str(), b.as_str(), c.as_str()]);
    }

    // 追加：重命名分组 = 整组移动；空新名 = 取消分组 -------------------------
    #[test]
    fn rename_group_moves_every_session_and_empty_name_ungroups() {
        let mut store = ResolveStore::default();
        let a = store.create_session(ResolveOrigin::Gui, 1);
        let b = store.create_session(ResolveOrigin::Gui, 2);
        let c = store.create_session(ResolveOrigin::Gui, 3);
        store.set_group(&a, Some("A"));
        store.set_group(&b, Some("A"));
        store.set_group(&c, Some("B"));
        assert_eq!(store.groups(), vec!["A".to_string(), "B".to_string()]);

        assert_eq!(store.rename_group("A", " Archive "), 2);
        assert_eq!(store.get(&a).unwrap().group.as_deref(), Some("Archive"));
        assert_eq!(store.get(&b).unwrap().group.as_deref(), Some("Archive"));
        assert_eq!(store.get(&c).unwrap().group.as_deref(), Some("B"));
        assert_eq!(store.groups(), vec!["Archive".to_string(), "B".to_string()]);

        // 同名 / 不存在的分组：无变更、返回 0
        assert_eq!(store.rename_group("Archive", "Archive"), 0);
        assert_eq!(store.rename_group("Missing", "X"), 0);
        assert_eq!(store.rename_group("   ", "X"), 0);

        // 空白新名 = 取消分组（整组回到「未分组」）
        assert_eq!(store.rename_group("Archive", "  "), 2);
        assert_eq!(store.get(&a).unwrap().group, None);
        assert_eq!(store.group_count(None), 2);
        assert_eq!(store.groups(), vec!["B".to_string()]);
    }

    // 追加：find_by_source_url 只按规范化 URL 命中，多条命中取最近更新 -------
    #[test]
    fn find_by_source_url_matches_normalized_page_url_only() {
        let mut store = ResolveStore::default();
        let first = store.create_session(ResolveOrigin::Gui, 1);
        {
            let session = store.get_mut(&first).unwrap();
            session.panel.source_url = "https://Example.com/Page?a=1#top".to_string();
            session.touch(10);
        }
        let other = store.create_session(ResolveOrigin::Extension, 2);
        store.get_mut(&other).unwrap().panel.source_url = "https://example.com/other".to_string();

        assert_eq!(
            store.find_id_by_source_url("https://example.com/Page?a=1"),
            Some(first.clone())
        );
        // query / 路径大小写不同 → 不命中
        assert!(store
            .find_by_source_url("https://example.com/Page?a=2")
            .is_none());
        assert!(store
            .find_by_source_url("https://example.com/page?a=1")
            .is_none());
        // 非法/非 http(s) → 不参与去重
        assert!(store.find_by_source_url("").is_none());
        assert!(store
            .find_by_source_url("ftp://example.com/Page?a=1")
            .is_none());

        // 同一规范化 URL 的多条历史：选最近更新的会话（用于复用）
        let latest = store.create_session(ResolveOrigin::Extension, 3);
        {
            let session = store.get_mut(&latest).unwrap();
            session.panel.source_url = "https://EXAMPLE.com/Page?a=1".to_string();
            session.touch(50);
        }
        assert_eq!(
            store.find_id_by_source_url("https://example.com/Page?a=1"),
            Some(latest)
        );
        // 空 source_url 的会话不会被误命中
        let blank = store.create_session(ResolveOrigin::Gui, 4);
        assert!(store.get(&blank).unwrap().panel.source_url.is_empty());
        assert!(store.find_by_source_url("").is_none());
    }

    // 追加：pinned/group 持久化往返 + 旧文件默认值 ---------------------------
    #[test]
    fn pinned_and_group_persist_and_old_files_use_defaults() {
        let dir = TempStateDir::new();
        let mut store = ResolveStore::load(dir.path());
        let pinned_id = store.create_session(ResolveOrigin::Gui, 100);
        let plain_id = store.create_session(ResolveOrigin::Extension, 200);
        store.set_pinned(&pinned_id, true);
        store.set_group(&pinned_id, Some(" 番剧 "));
        assert!(store.save_if_dirty(dir.path()).unwrap());

        let json = read_file_json(dir.path());
        let sessions = json["sessions"].as_array().expect("sessions 是数组");
        assert_eq!(sessions[1]["id"], json!(pinned_id));
        assert_eq!(sessions[1]["pinned"], json!(true));
        assert_eq!(sessions[1]["group"], json!("番剧"));
        assert_eq!(sessions[0]["id"], json!(plain_id));
        assert_eq!(sessions[0]["pinned"], json!(false));
        assert_eq!(sessions[0]["group"], json!(null));

        let loaded = ResolveStore::load(dir.path());
        let pinned = loaded.get(&pinned_id).expect("置顶会话应被加载");
        assert!(pinned.pinned);
        assert_eq!(pinned.group.as_deref(), Some("番剧"));
        let plain = loaded.get(&plain_id).expect("普通会话应被加载");
        assert!(!plain.pinned);
        assert_eq!(plain.group, None);
        assert_eq!(loaded.sorted_sessions()[0].id, pinned_id);

        // 旧文件（没有 pinned/group 字段）→ 默认值；空白分组归一化为未分组
        let path = resolve_sessions_path(dir.path());
        std::fs::write(
            &path,
            r#"{"sessions":[{"id":"rs_000009","title":"旧会话","status":"ready"},{"id":"rs_000010","title":"旧会话2","group":"  "}]}"#,
        )
        .unwrap();
        let old = ResolveStore::load(dir.path());
        assert!(!old.get("rs_000009").unwrap().pinned);
        assert_eq!(old.get("rs_000009").unwrap().group, None);
        assert_eq!(old.get("rs_000010").unwrap().group, None);
        assert!(old.groups().is_empty());
    }
}
