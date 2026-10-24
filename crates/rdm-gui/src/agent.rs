//! GUI 下载 Agent：把「自然语言 + 链接」变成**可执行工作流**（计划 → 工具执行 → 结果汇报）。
//!
//! 背景：早期「检查器助手」页只有聊天，用户粘贴 B 站链接时模型只能给出手动操作教程；
//! 2026-10-22 起该能力迁到独立的「资源解析」页面（`views/app.rs`），按会话保存历史。
//! 本模块提供纯逻辑（无 IO、无网络、无 gpui 依赖）：
//!
//! 1. [`extract_urls`] 从消息里识别 http(s) 链接；
//! 2. [`route_turn`] 判断这一轮该「执行工作流」还是「普通问答」；
//! 3. [`AgentPlan`] 记录步骤时间线（理解请求 / 解析链接 / 创建下载任务 / 生成汇报）与状态；
//! 4. 解析/下载完成后由 [`resolve_summary`]/[`download_summary`] 生成确定性汇报，
//!    或由 [`summary_prompt`] 交给模型做**执行结果汇报**（模型不决定是否执行）。
//!
//! 真正的工具执行在 `views/app.rs`（复用既有 `EngineRuntime::start_ai_parse` 与
//! `create_downloads_for` 链路）；本模块只保证计划/步骤/文案可单测、可复现。

use crate::model::{AiPanelState, AiParseStatus, AiResourceCard};

/// 一条消息最多识别几个链接（只对第一个执行工作流，其余在步骤详情里提示）。
pub const MAX_PROMPT_URLS: usize = 3;

/// 步骤时间线最多保留的步骤数（防御性上限）。
pub const MAX_STEPS: usize = 4;

/// 工作流里的一步（对应一次「工具」执行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTool {
    /// 理解用户请求（本地、瞬时完成）。
    Understand,
    /// 解析链接（`rdm-resolve` resolver-first，失败时 legacy 兜底）。
    Resolve,
    /// 创建下载任务。
    Download,
    /// 生成执行结果汇报（模型或确定性文案）。
    Summarize,
}

impl AgentTool {
    pub fn title(self) -> &'static str {
        match self {
            Self::Understand => "理解请求",
            Self::Resolve => "解析链接",
            Self::Download => "创建下载任务",
            Self::Summarize => "生成汇报",
        }
    }
}

/// 步骤状态；`Running` 用于渲染「进行中」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Pending => "等待",
            Self::Running => "进行中",
            Self::Done => "完成",
            Self::Failed => "失败",
            Self::Skipped => "跳过",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Skipped)
    }

    #[allow(dead_code)]
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }
}

/// 工作流里的一步：标题 + 状态 + 结果摘要 + 起止时间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStep {
    pub tool: AgentTool,
    pub status: StepStatus,
    /// 展示给用户的一行结果（不含 URL 凭证；解析 URL 只展示 host）。
    pub detail: Option<String>,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
}

impl AgentStep {
    pub fn new(tool: AgentTool) -> Self {
        Self {
            tool,
            status: StepStatus::Pending,
            detail: None,
            started_ms: None,
            finished_ms: None,
        }
    }

    /// 已耗时（毫秒）：进行中按 `now_ms` 计算，终态按完成时间。
    pub fn duration_ms(&self, now_ms: u64) -> Option<u64> {
        let started = self.started_ms?;
        let finished = self.finished_ms.unwrap_or(now_ms);
        Some(finished.saturating_sub(started))
    }
}

/// 整条工作流的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPlanState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentPlanState {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Running => "执行中",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已取消",
        }
    }
}

/// 「下载已有资源」时的选择方式（自然语言后续指令）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadSelection {
    /// 全部资源。
    All,
    /// 当前勾选的资源。
    Selected,
    /// 第 N 个（1-based）。
    Index(usize),
    /// 按资源类型筛选。
    Kind(KindFilter),
}

/// 资源类型筛选（与 `rdm_ai::ParsedResource.kind` 的英文值对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KindFilter {
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Program,
    Other,
}

impl KindFilter {
    pub fn resource_kind(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Image => "image",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Program => "program",
            Self::Other => "other",
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Video => "视频",
            Self::Audio => "音频",
            Self::Image => "图片",
            Self::Document => "文档",
            Self::Archive => "压缩包",
            Self::Program => "程序",
            Self::Other => "其它",
        }
    }
}

/// 这一轮消息要执行的工作流类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIntent {
    /// 只解析（展示资源，不自动下载）。
    Resolve,
    /// 解析成功后自动创建下载任务。
    ResolveAndDownload,
    /// 没有新链接：对上一次解析结果执行下载。
    DownloadExisting(DownloadSelection),
    /// 普通问答（没有可执行目标）。
    Chat,
}

impl AgentIntent {
    pub fn wants_auto_download(self) -> bool {
        matches!(self, Self::ResolveAndDownload)
    }
}

/// 执行计划：步骤时间线 + 状态 + 结果摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPlan {
    /// 本轮 AI 解析请求代号（解析开始后由 app 填入；`DownloadExisting` 恒为 0）。
    pub request_id: u64,
    pub intent: AgentIntent,
    /// 本轮要解析的 URL（`DownloadExisting` 为空）。
    pub source_url: String,
    /// 用户原始消息（用于汇报 prompt）。
    pub user_text: String,
    pub steps: Vec<AgentStep>,
    pub state: AgentPlanState,
    /// 确定性汇报（AI 未配置/汇报失败时的兜底，同时也是最终结果的快照）。
    pub summary: Option<String>,
    /// 已创建的下载任务名（≤8 个展示）。
    pub created_downloads: Vec<String>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

impl AgentPlan {
    /// 按意图生成步骤时间线；「理解请求」立即完成。
    pub fn new(intent: AgentIntent, source_url: &str, user_text: &str, now_ms: u64) -> Self {
        let mut steps: Vec<AgentStep> = Vec::new();
        let mut understand = AgentStep::new(AgentTool::Understand);
        understand.status = StepStatus::Done;
        understand.started_ms = Some(now_ms);
        understand.finished_ms = Some(now_ms);
        understand.detail = Some(match intent {
            AgentIntent::DownloadExisting(_) => "沿用上一次解析结果".to_string(),
            _ => {
                let host = pretty_host(source_url);
                if host.is_empty() {
                    "识别到下载请求".to_string()
                } else {
                    format!("识别到链接：{host}")
                }
            }
        });
        steps.push(understand);

        match intent {
            AgentIntent::Resolve => {
                let mut resolve = AgentStep::new(AgentTool::Resolve);
                resolve.status = StepStatus::Running;
                resolve.started_ms = Some(now_ms);
                steps.push(resolve);
                steps.push(AgentStep::new(AgentTool::Summarize));
            }
            AgentIntent::ResolveAndDownload => {
                let mut resolve = AgentStep::new(AgentTool::Resolve);
                resolve.status = StepStatus::Running;
                resolve.started_ms = Some(now_ms);
                steps.push(resolve);
                steps.push(AgentStep::new(AgentTool::Download));
                steps.push(AgentStep::new(AgentTool::Summarize));
            }
            AgentIntent::DownloadExisting(_) => {
                let mut download = AgentStep::new(AgentTool::Download);
                download.status = StepStatus::Running;
                download.started_ms = Some(now_ms);
                steps.push(download);
                steps.push(AgentStep::new(AgentTool::Summarize));
            }
            AgentIntent::Chat => {}
        }

        steps.truncate(MAX_STEPS);
        Self {
            request_id: 0,
            intent,
            source_url: source_url.to_string(),
            user_text: user_text.to_string(),
            steps,
            state: AgentPlanState::Running,
            summary: None,
            created_downloads: Vec::new(),
            started_ms: now_ms,
            finished_ms: None,
        }
    }

    #[allow(dead_code)]
    pub fn step(&self, tool: AgentTool) -> Option<&AgentStep> {
        self.steps.iter().find(|step| step.tool == tool)
    }

    pub fn step_mut(&mut self, tool: AgentTool) -> Option<&mut AgentStep> {
        self.steps.iter_mut().find(|step| step.tool == tool)
    }

    /// 标记某一步为进行中（幂等；不覆盖终态）。
    pub fn start_step(&mut self, tool: AgentTool, now_ms: u64) {
        if let Some(step) = self.step_mut(tool) {
            if step.status.is_terminal() {
                return;
            }
            step.status = StepStatus::Running;
            if step.started_ms.is_none() {
                step.started_ms = Some(now_ms);
            }
        }
    }

    /// 结束某一步：写状态/结果摘要/完成时间。
    pub fn finish_step(
        &mut self,
        tool: AgentTool,
        status: StepStatus,
        detail: Option<String>,
        now_ms: u64,
    ) {
        if let Some(step) = self.step_mut(tool) {
            step.status = status;
            if step.started_ms.is_none() {
                step.started_ms = Some(now_ms);
            }
            step.finished_ms = Some(now_ms);
            if let Some(detail) = detail {
                step.detail = Some(detail);
            }
        }
    }

    /// 标记计划失败：当前进行中的步骤置为失败，未执行的步骤置为跳过。
    pub fn fail(&mut self, detail: impl Into<String>, now_ms: u64) {
        let detail = detail.into();
        let mut first = true;
        for step in self.steps.iter_mut() {
            match step.status {
                StepStatus::Running => {
                    step.status = StepStatus::Failed;
                    step.finished_ms = Some(now_ms);
                    if first {
                        step.detail = Some(detail.clone());
                        first = false;
                    }
                }
                StepStatus::Pending => {
                    step.status = StepStatus::Skipped;
                    step.finished_ms = Some(now_ms);
                }
                _ => {}
            }
        }
        self.state = AgentPlanState::Failed;
        self.finished_ms = Some(now_ms);
        self.summary = Some(failure_summary(&detail));
    }

    /// 标记计划完成（非终态步骤补 Done/Skipped）。
    pub fn complete(&mut self, detail: Option<String>, now_ms: u64) {
        if let Some(detail) = detail {
            if let Some(step) = self.step_mut(AgentTool::Summarize) {
                if step.status != StepStatus::Done {
                    step.status = StepStatus::Done;
                }
                step.detail.get_or_insert(detail);
            }
        }
        for step in self.steps.iter_mut() {
            if step.status.is_terminal() {
                continue;
            }
            step.status = match step.status {
                StepStatus::Running => StepStatus::Done,
                _ => StepStatus::Skipped,
            };
            step.finished_ms.get_or_insert(now_ms);
        }
        self.state = AgentPlanState::Completed;
        self.finished_ms = Some(now_ms);
    }

    /// 取消（用户主动停止 / 关闭会话）：未完成步骤置 `Skipped`，计划状态置
    /// [`AgentPlanState::Cancelled`]。这不是「完成」——UI 必须显示「已取消」，
    /// 未执行的步骤不得伪装成功（审计 F-20 修正了旧注释的语义错误）。
    pub fn cancel(&mut self, detail: impl Into<String>, now_ms: u64) {
        let detail = detail.into();
        for step in self.steps.iter_mut() {
            if !step.status.is_terminal() {
                step.status = StepStatus::Skipped;
                step.finished_ms = Some(now_ms);
                step.detail.get_or_insert_with(|| detail.clone());
            }
        }
        self.state = AgentPlanState::Cancelled;
        self.finished_ms = Some(now_ms);
    }

    pub fn is_running(&self) -> bool {
        self.state == AgentPlanState::Running
    }

    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.finished_ms
            .unwrap_or(now_ms)
            .saturating_sub(self.started_ms)
    }

    /// 步骤进度（已终态数 / 总数）。
    pub fn progress_label(&self) -> String {
        let total = self.steps.len();
        let done = self
            .steps
            .iter()
            .filter(|step| step.status.is_terminal())
            .count();
        format!("{done}/{total}")
    }

    #[allow(dead_code)]
    pub fn current_tool(&self) -> Option<AgentTool> {
        self.steps
            .iter()
            .find(|step| step.status == StepStatus::Running)
            .map(|step| step.tool)
    }
}

/// 一次用户输入的路由结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTurn {
    /// 没有可执行目标：普通问答（prompt 已带上下文与 Agent 约束）。
    Chat { prompt: String },
    /// 执行工作流：`url` 非空 → 先解析；为空 → 对已有资源直接下载。
    Plan {
        plan: AgentPlan,
        url: Option<String>,
        selection: Option<DownloadSelection>,
    },
}

/// 路由一轮用户消息：识别链接/下载意图，决定执行工作流还是普通问答。
///
/// - 消息里有 http(s) 链接：`下载`意图 → `ResolveAndDownload`，否则 `Resolve`；
/// - 没有链接但已有一轮解析结果：`下载第 2 个/全部下载/只要音频` → `DownloadExisting`；
/// - 其它 → `Chat`（不再假装执行）。
pub fn route_turn(text: &str, panel: &AiPanelState, now_ms: u64) -> AgentTurn {
    let text = text.trim();
    let urls = extract_urls(text);
    if let Some(url) = urls.first() {
        let intent = classify_intent(text, true, false);
        let mut plan = AgentPlan::new(intent, url, text, now_ms);
        // 多链接提示（只对第一个执行，避免一次消息批量误下载）。
        if urls.len() > 1 {
            if let Some(step) = plan.step_mut(AgentTool::Understand) {
                step.detail = Some(format!(
                    "识别到 {} 个链接，先处理：{}",
                    urls.len(),
                    pretty_host(url)
                ));
            }
        }
        let url = Some(url.clone());
        return AgentTurn::Plan {
            plan,
            url,
            selection: None,
        };
    }

    let has_resources = panel.status == AiParseStatus::Ready && !panel.resources.is_empty();
    if has_resources {
        if let Some(selection) = parse_download_selection(text) {
            if !matches!(selection, DownloadSelection::Selected) || has_selection(panel) {
                let intent = AgentIntent::DownloadExisting(selection);
                let source_url = panel.source_url.clone();
                let plan = AgentPlan::new(intent, &source_url, text, now_ms);
                return AgentTurn::Plan {
                    plan,
                    url: None,
                    selection: Some(selection),
                };
            }
        }
    }

    AgentTurn::Chat {
        prompt: chat_prompt(text, panel),
    }
}

/// 从消息里提取 http(s) 链接：去重、去 fragment、去掉尾部中文/标点、最多 [`MAX_PROMPT_URLS`] 条。
pub fn extract_urls(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut urls: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let rest = &text[index..];
        let lower = rest.to_ascii_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            // 跳到下一个可能位置（按字节安全前进一个 char）。
            let ch = rest.chars().next().unwrap_or('\0');
            index += ch.len_utf8();
            continue;
        }

        let start = index;
        let mut end = start;
        for (offset, ch) in text[start..].char_indices() {
            if ch.is_ascii_whitespace() || !ch.is_ascii() {
                end = start + offset;
                break;
            }
            end = start + offset + ch.len_utf8();
        }
        let mut candidate = &text[start..end];

        // 去掉尾部标点（URL 合法字符之外、或常见句子标点）。
        loop {
            let trimmed = candidate.trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | ')'
                        | ']'
                        | '}'
                        | '>'
                        | '"'
                        | '\''
                        | '`'
                        | '。'
                        | '，'
                        | '；'
                        | '：'
                        | '！'
                        | '？'
                        | '）'
                        | '】'
                        | '》'
                        | '、'
                )
            });
            if trimmed.len() == candidate.len() {
                break;
            }
            candidate = trimmed;
        }

        if let Ok(parsed) = url::Url::parse(candidate) {
            if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() {
                let mut normalized = parsed.to_string();
                if parsed.fragment().is_some() {
                    // `Url::to_string` 已含 fragment；去掉它只保留资源定位。
                    normalized = normalized
                        .split('#')
                        .next()
                        .unwrap_or(&normalized)
                        .to_string();
                }
                if !urls.contains(&normalized) {
                    urls.push(normalized);
                }
                if urls.len() >= MAX_PROMPT_URLS {
                    break;
                }
            }
        }
        index = end.max(start + 1);
    }
    urls
}

/// 判断消息的下载/解析意图。
pub fn classify_intent(text: &str, has_url: bool, has_resources: bool) -> AgentIntent {
    let lower = text.to_lowercase();
    let wants_download = contains_any(&lower, &DOWNLOAD_WORDS);
    if has_url {
        if wants_download {
            AgentIntent::ResolveAndDownload
        } else {
            AgentIntent::Resolve
        }
    } else if has_resources {
        if let Some(selection) = parse_download_selection(text) {
            AgentIntent::DownloadExisting(selection)
        } else if wants_download {
            AgentIntent::DownloadExisting(DownloadSelection::All)
        } else {
            AgentIntent::Chat
        }
    } else {
        AgentIntent::Chat
    }
}

const DOWNLOAD_WORDS: [&str; 15] = [
    "下载",
    "下到",
    "帮我下",
    "下一下",
    "保存",
    "存到",
    "拉到",
    "拉取",
    "离线",
    "download",
    "get it",
    "grab",
    "抓取",
    "收下",
    "弄下来",
];

const ALL_WORDS: [&str; 9] = [
    "全部下载",
    "全都下载",
    "都下载",
    "全下",
    "全都要",
    "下载全部",
    "下载所有",
    "所有都下",
    "download all",
];

const SELECTED_WORDS: [&str; 6] = [
    "下载所选",
    "选中的",
    "勾选的",
    "所选的",
    "选中项",
    "selected",
];

const KIND_WORDS: [(&str, KindFilter); 16] = [
    ("视频", KindFilter::Video),
    ("影片", KindFilter::Video),
    ("video", KindFilter::Video),
    ("音频", KindFilter::Audio),
    ("声音", KindFilter::Audio),
    ("audio", KindFilter::Audio),
    ("图片", KindFilter::Image),
    ("图像", KindFilter::Image),
    ("海报", KindFilter::Image),
    ("image", KindFilter::Image),
    ("文档", KindFilter::Document),
    ("document", KindFilter::Document),
    ("压缩", KindFilter::Archive),
    ("archive", KindFilter::Archive),
    ("安装包", KindFilter::Program),
    ("program", KindFilter::Program),
];

/// 解析「下载第 2 个 / 全部下载 / 只要音频」这类后续指令；没有选择指令返回 `None`。
pub fn parse_download_selection(text: &str) -> Option<DownloadSelection> {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    if contains_any(&lower, &ALL_WORDS) {
        return Some(DownloadSelection::All);
    }
    if contains_any(&lower, &SELECTED_WORDS) {
        return Some(DownloadSelection::Selected);
    }
    if let Some(index) = parse_ordinal(&lower) {
        return Some(DownloadSelection::Index(index));
    }
    // 类型筛选需要与下载/只要类词同时出现，避免把「这个视频不错」误判为下载指令。
    let kind_trigger = contains_any(&lower, &["下载", "只要", "只下", "仅要", "只保留", "仅"]);
    if kind_trigger {
        for (word, kind) in KIND_WORDS {
            if lower.contains(word) {
                return Some(DownloadSelection::Kind(kind));
            }
        }
    }
    None
}

/// 解析「第 2 个 / 第 2 条 / 第二个」里的序号（1-based）。
fn parse_ordinal(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    for (index, ch) in chars.iter().enumerate() {
        if *ch != '第' {
            continue;
        }
        let mut cursor = index + 1;
        let mut digits = String::new();
        let mut chinese: Option<usize> = None;
        let mut saw_ten = false;
        while cursor < chars.len() {
            let current = chars[cursor];
            if current.is_ascii_whitespace() {
                // 允许「第 2 个」这种带空格的写法。
                cursor += 1;
                continue;
            }
            if current.is_ascii_digit() {
                digits.push(current);
                cursor += 1;
                continue;
            }
            if current == '十' {
                if saw_ten || !digits.is_empty() {
                    break;
                }
                saw_ten = true;
                chinese = Some(chinese.unwrap_or(1) * 10);
                cursor += 1;
                continue;
            }
            if let Some(value) = chinese_digit(current) {
                if saw_ten && chinese.is_some_and(|base| base >= 10) {
                    // 二十一 / 二十五：十位之后累加个位。
                    chinese = chinese.map(|base| base + value);
                } else {
                    chinese = Some(chinese.unwrap_or(0) + value);
                }
                cursor += 1;
                continue;
            }
            break;
        }
        let value = if let Ok(value) = digits.parse::<usize>() {
            Some(value)
        } else {
            chinese
        };
        if let Some(value) = value {
            if (1..=99).contains(&value) {
                // 序号后应跟量词或结束（避免「第 2 步」误判）；量词前允许空格。
                let mut lookahead = cursor;
                while lookahead < chars.len() && chars[lookahead].is_ascii_whitespace() {
                    lookahead += 1;
                }
                let next = chars.get(lookahead).copied();
                if next.is_none()
                    || matches!(
                        next,
                        Some('个' | '条' | '项' | '份' | '集' | '首' | '张' | '部')
                    )
                {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn chinese_digit(ch: char) -> Option<usize> {
    match ch {
        '一' => Some(1),
        '二' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        '十' => Some(10),
        _ => None,
    }
}

fn contains_any(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

fn has_selection(panel: &AiPanelState) -> bool {
    panel.resources.iter().any(|card| card.selected)
}

/// 选择要下载的资源下标（供 app 复用现有 `download_ai_selected` 链路）。
pub fn selected_resource_indices(
    resources: &[AiResourceCard],
    selection: DownloadSelection,
) -> Vec<usize> {
    match selection {
        DownloadSelection::All => (0..resources.len()).collect(),
        DownloadSelection::Selected => resources
            .iter()
            .enumerate()
            .filter(|(_, card)| card.selected)
            .map(|(index, _)| index)
            .collect(),
        DownloadSelection::Index(index) => {
            let index = index.saturating_sub(1);
            if index < resources.len() {
                vec![index]
            } else {
                Vec::new()
            }
        }
        DownloadSelection::Kind(kind) => resources
            .iter()
            .enumerate()
            .filter(|(_, card)| {
                card.resource
                    .kind
                    .eq_ignore_ascii_case(kind.resource_kind())
                    || (kind == KindFilter::Other
                        && !matches!(
                            card.resource.kind.as_str(),
                            "video" | "audio" | "image" | "document" | "archive" | "program"
                        ))
            })
            .map(|(index, _)| index)
            .collect(),
    }
}

/// 解析完成后的确定性汇报（AI 未配置/模型汇报失败时的兜底；不教用户手动操作）。
pub fn resolve_summary(
    plan: &AgentPlan,
    report: &rdm_ai::ParseReport,
    created: &[String],
) -> String {
    if report.resources.is_empty() {
        let mut message = String::from("没有解析到可下载资源。");
        let reasons: Vec<&str> = report
            .notes
            .iter()
            .map(|note| note.trim())
            .filter(|note| !note.is_empty())
            .take(3)
            .collect();
        if !reasons.is_empty() {
            message.push_str("原因：");
            message.push_str(&reasons.join("；"));
        }
        message.push_str(
            "你可以点击「重试解析」；需要登录/权限的页面，请用浏览器扩展「解析页面资源」带上当前登录态重试。",
        );
        return message;
    }

    let mut kinds: Vec<String> = Vec::new();
    for resource in &report.resources {
        let label = resource_kind_label(&resource.kind).to_string();
        if !kinds.iter().any(|kind| kind == &label) {
            kinds.push(label);
        }
    }
    let mut message = format!(
        "已解析到 {} 个资源（{}）。",
        report.resources.len(),
        kinds.join("、")
    );
    if plan.intent.wants_auto_download() {
        if created.is_empty() {
            message.push_str("下载任务创建失败：请检查保存目录设置后重试。");
        } else {
            message.push_str(&format!(
                "已自动创建 {} 个下载任务：{}。",
                created.len(),
                joined_names(created)
            ));
        }
    } else if !created.is_empty() {
        message.push_str(&format!(
            "已创建 {} 个下载任务：{}。",
            created.len(),
            joined_names(created)
        ));
    } else {
        message.push_str("可以在上方勾选资源后点击「下载全部」或「下载所选」创建任务。");
    }
    let notes: Vec<&str> = report
        .notes
        .iter()
        .map(|note| note.trim())
        .filter(|note| !note.is_empty())
        .take(1)
        .collect();
    if let Some(note) = notes.first() {
        message.push_str(&format!("（{note}）"));
    }
    message
}

/// 下载已有资源后的确定性汇报。
pub fn download_summary(selection: DownloadSelection, source: &str, created: &[String]) -> String {
    if created.is_empty() {
        return match selection {
            DownloadSelection::Kind(kind) => {
                format!(
                    "没有找到{}资源；请换一个筛选条件，或先重新解析。",
                    kind.label_zh()
                )
            }
            DownloadSelection::Index(index) => {
                format!("没有第 {index} 个资源；当前解析结果数量不足。")
            }
            DownloadSelection::Selected => "当前没有勾选任何资源。".to_string(),
            DownloadSelection::All => "当前没有可下载的资源，请先解析链接。".to_string(),
        };
    }
    let mut message = format!(
        "已创建 {} 个下载任务：{}。",
        created.len(),
        joined_names(created)
    );
    if !source.is_empty() {
        message.push_str(&format!("来源：{}。", pretty_host(source)));
    }
    message.push_str("可以在左侧任务列表查看进度。");
    message
}

/// 解析失败时的确定性汇报（notes 只取前 3 条，不把推理链/凭证塞给用户）。
pub fn failure_summary(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() {
        return "解析失败（没有返回原因）；可以点击「重试解析」。".to_string();
    }
    format!("解析失败：{detail}。可以点击「重试解析」。")
}

/// 模型 system prompt：Agent 只汇报执行结果，不教用户手动操作。
pub fn summary_system_prompt() -> String {
    "你是 RDM（Rust Download Manager）桌面端的下载 Agent。\
     链接解析、资源校验与下载任务创建已经由客户端工具真正执行完毕；\
     你只需要用中文简要汇报执行结果和当前状态，并最多给出 2 个可选的下一步。\
     规则：不要教用户手动复制链接、手动解析或手动添加任务；不要输出分步操作教程；\
     不要编造未执行的步骤、未解析出的资源、未创建的任务或不存在的清晰度/大小；\
     没有资源就直接说明失败原因。总长度不超过 120 字。"
        .to_string()
}

/// 模型 user prompt：带执行事实（步骤/资源/已创建任务），禁止编造。
pub fn summary_prompt(user_text: &str, plan: &AgentPlan, report: &rdm_ai::ParseReport) -> String {
    let mut prompt = String::new();
    prompt.push_str("用户请求：");
    prompt.push_str(&scrub_urls(user_text.trim()));
    prompt.push_str("\n\n已执行的工作流（由客户端工具完成，你不需要再次调用工具）：");
    for step in &plan.steps {
        prompt.push_str(&format!(
            "\n- {}：{}{}",
            step.tool.title(),
            step.status.label_zh(),
            step.detail
                .as_deref()
                .map(|detail| format!("（{detail}）"))
                .unwrap_or_default()
        ));
    }
    prompt.push_str("\n\n解析结果：");
    prompt.push_str(&format!("\n- 来源：{}", pretty_host(&report.source_url)));
    prompt.push_str(&format!("\n- 资源数：{}", report.resources.len()));
    for resource in report.resources.iter().take(8) {
        prompt.push_str(&format!(
            "\n  - {}｜{}｜{}",
            resource.name.trim(),
            resource_kind_label(&resource.kind),
            resource
                .size_bytes
                .filter(|size| *size > 0)
                .map(format_bytes)
                .unwrap_or_else(|| "大小未知".to_string())
        ));
    }
    if !plan.created_downloads.is_empty() {
        prompt.push_str(&format!(
            "\n- 已创建下载任务：{}",
            joined_names(&plan.created_downloads)
        ));
    }
    if !report.notes.is_empty() {
        prompt.push_str("\n- 解析说明：");
        prompt.push_str(
            &report
                .notes
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("；"),
        );
    }
    prompt.push_str("\n\n请只输出给用户的一段中文汇报（不要标题/编号列表/Markdown）。");
    prompt
}

/// 普通问答的 system prompt：没有可执行目标时不要假装执行。
pub fn chat_system_prompt() -> String {
    "你是 RDM（Rust Download Manager）桌面端的下载 Agent。\
     你可以执行的能力只有：解析 http(s) 链接、把解析出的资源创建为下载任务。\
     如果用户这轮没有提供链接、也没有可下载的解析结果，就用一句话请用户粘贴链接；\
     不要输出大段手动操作教程，不要假装已经解析或下载。\
     用户问配置/使用问题时用中文简短回答，可以指向设置页或任务列表。"
        .to_string()
}

/// 普通问答 prompt：附带当前解析会话摘要（帮助模型回答「有哪些资源/怎么下载」）。
pub fn chat_prompt(user_text: &str, panel: &AiPanelState) -> String {
    let mut prompt = String::new();
    prompt.push_str("用户消息：");
    prompt.push_str(&scrub_urls(user_text.trim()));
    prompt.push_str("\n\n当前会话：");
    prompt.push_str(&format!(
        "\n- 解析状态：{}",
        panel.status.clone().label_zh()
    ));
    if !panel.source_url.is_empty() {
        prompt.push_str(&format!(
            "\n- 已解析来源：{}",
            pretty_host(&panel.source_url)
        ));
    }
    if !panel.resources.is_empty() {
        prompt.push_str(&format!("\n- 资源数：{}", panel.resources.len()));
        for card in panel.resources.iter().take(8) {
            prompt.push_str(&format!(
                "\n  - {}（{}）",
                card.resource.name.trim(),
                resource_kind_label(&card.resource.kind)
            ));
        }
    }
    if let Some(plan) = panel.last_plan_summary.as_deref() {
        prompt.push_str(&format!("\n- 最近一次工作流：{plan}"));
    }
    prompt.push_str("\n\n如果用户要求下载但没给链接，就请用户粘贴链接；不要编造执行结果。");
    prompt
}

/// 资源类型 → 中文标签。
pub fn resource_kind_label(kind: &str) -> &'static str {
    match kind {
        "video" => "视频",
        "audio" => "音频",
        "image" => "图片",
        "document" => "文档",
        "archive" => "压缩包",
        "program" => "程序",
        _ => "其它",
    }
}

/// 把消息里的完整 URL 替换成 host 占位（发给模型前脱敏 query/fragment/凭证）。
fn scrub_urls(text: &str) -> String {
    let mut out = text.to_string();
    for url in extract_urls(text) {
        let host = pretty_host(&url);
        let replacement = if host.is_empty() {
            "[链接]".to_string()
        } else {
            format!("[链接 {}]", host)
        };
        out = out.replace(&url, &replacement);
    }
    out
}

/// URL → host（无 host 时截断原串，绝不显示 query/凭证）。
pub fn pretty_host(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return String::new();
    }
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            return host.trim_end_matches('.').to_ascii_lowercase();
        }
    }
    let short: String = url.chars().take(60).collect();
    short
}

fn joined_names(names: &[String]) -> String {
    let valid: Vec<&str> = names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    if valid.is_empty() {
        return "未命名任务".to_string();
    }
    let mut text = valid.iter().take(4).copied().collect::<Vec<_>>().join("、");
    if valid.len() > 4 {
        text.push_str(&format!(" 等 {} 个", valid.len()));
    }
    text
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(resources: Vec<(&str, &str)>, notes: &[&str]) -> rdm_ai::ParseReport {
        rdm_ai::ParseReport {
            source_url: "https://example.com/page".to_string(),
            source_kind: "html_page".to_string(),
            title: Some("示例".to_string()),
            resources: resources
                .into_iter()
                .map(|(name, kind)| rdm_ai::ParsedResource {
                    url: format!("https://cdn.example.com/{name}"),
                    name: name.to_string(),
                    kind: kind.to_string(),
                    size_bytes: Some(1024 * 1024),
                    content_type: None,
                    note: None,
                })
                .collect(),
            notes: notes.iter().map(|note| note.to_string()).collect(),
            ai_used: false,
            model: None,
            usage: rdm_ai::TokenUsage::default(),
            created_ms: 1,
            cache_hit: false,
            failure_kind: None,
        }
    }

    fn panel_with_resources() -> AiPanelState {
        AiPanelState {
            status: AiParseStatus::Ready,
            source_url: "https://example.com/page".to_string(),
            resources: vec![
                AiResourceCard {
                    resource: rdm_ai::ParsedResource {
                        url: "https://cdn.example.com/a.mp4".to_string(),
                        name: "a.mp4".to_string(),
                        kind: "video".to_string(),
                        size_bytes: None,
                        content_type: None,
                        note: None,
                    },
                    selected: true,
                },
                AiResourceCard {
                    resource: rdm_ai::ParsedResource {
                        url: "https://cdn.example.com/b.m4a".to_string(),
                        name: "b.m4a".to_string(),
                        kind: "audio".to_string(),
                        size_bytes: None,
                        content_type: None,
                        note: None,
                    },
                    selected: false,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn extracts_plain_urls_and_strips_punctuation() {
        assert_eq!(
            extract_urls("帮我解析这个：https://www.bilibili.com/video/BV1snc6qEt5/。"),
            vec!["https://www.bilibili.com/video/BV1snc6qEt5/".to_string()]
        );
        assert_eq!(
            extract_urls("看这个 https://example.com/a.zip，然后下载"),
            vec!["https://example.com/a.zip".to_string()]
        );
        assert_eq!(
            extract_urls("https://example.com/a.zip?x=1#frag"),
            vec!["https://example.com/a.zip?x=1".to_string()]
        );
    }

    #[test]
    fn extracts_multiple_urls_dedup_and_limits() {
        let urls = extract_urls(
            "https://a.example/1 https://a.example/1 https://b.example/2 https://c.example/3 https://d.example/4",
        );
        assert_eq!(
            urls,
            vec![
                "https://a.example/1".to_string(),
                "https://b.example/2".to_string(),
                "https://c.example/3".to_string()
            ]
        );
    }

    #[test]
    fn ignores_non_http_and_bare_hosts() {
        assert!(extract_urls("example.com/a.zip").is_empty());
        assert!(extract_urls("ftp://example.com/a.zip").is_empty());
        assert!(extract_urls("javascript:alert(1)").is_empty());
        assert!(extract_urls("没有链接").is_empty());
    }

    #[test]
    fn stops_at_chinese_characters_without_spaces() {
        assert_eq!(
            extract_urls("下载这个https://example.com/a.mp4谢谢"),
            vec!["https://example.com/a.mp4".to_string()]
        );
    }

    #[test]
    fn classifies_resolve_vs_resolve_and_download() {
        assert_eq!(
            classify_intent("解析这个链接", true, false),
            AgentIntent::Resolve
        );
        assert_eq!(
            classify_intent("帮我下载这个", true, false),
            AgentIntent::ResolveAndDownload
        );
        assert_eq!(
            classify_intent("看看有什么资源", true, false),
            AgentIntent::Resolve
        );
        assert_eq!(
            classify_intent("download it", true, false),
            AgentIntent::ResolveAndDownload
        );
    }

    #[test]
    fn classifies_followup_downloading_when_resources_exist() {
        assert_eq!(
            classify_intent("下载第 2 个", false, true),
            AgentIntent::DownloadExisting(DownloadSelection::Index(2))
        );
        assert_eq!(
            classify_intent("全部下载", false, true),
            AgentIntent::DownloadExisting(DownloadSelection::All)
        );
        assert_eq!(
            classify_intent("只要音频", false, true),
            AgentIntent::DownloadExisting(DownloadSelection::Kind(KindFilter::Audio))
        );
    }

    #[test]
    fn ignores_questions_without_download_intent() {
        assert_eq!(
            classify_intent("这个视频清晰度怎么样", false, true),
            AgentIntent::Chat
        );
        assert_eq!(classify_intent("你好", false, false), AgentIntent::Chat);
    }

    #[test]
    fn parses_download_selection_variants() {
        assert_eq!(
            parse_download_selection("下载第2个"),
            Some(DownloadSelection::Index(2))
        );
        assert_eq!(
            parse_download_selection("第二个"),
            Some(DownloadSelection::Index(2))
        );
        assert_eq!(
            parse_download_selection("第 2 个文件"),
            Some(DownloadSelection::Index(2))
        );
        assert_eq!(
            parse_download_selection("全部下载"),
            Some(DownloadSelection::All)
        );
        assert_eq!(
            parse_download_selection("下载所选"),
            Some(DownloadSelection::Selected)
        );
        assert_eq!(
            parse_download_selection("只要音频"),
            Some(DownloadSelection::Kind(KindFilter::Audio))
        );
        assert_eq!(parse_download_selection("第 2 步怎么做"), None);
        assert_eq!(parse_download_selection("你好"), None);
    }

    #[test]
    fn route_turn_starts_a_plan_for_a_url() {
        let panel = AiPanelState::default();
        match route_turn("帮我下载 https://example.com/a.zip", &panel, 100) {
            AgentTurn::Plan {
                plan,
                url,
                selection,
            } => {
                assert_eq!(url.as_deref(), Some("https://example.com/a.zip"));
                assert_eq!(plan.intent, AgentIntent::ResolveAndDownload);
                assert_eq!(plan.steps.len(), 4);
                assert_eq!(plan.steps[0].status, StepStatus::Done);
                assert_eq!(plan.steps[1].tool, AgentTool::Resolve);
                assert_eq!(plan.steps[1].status, StepStatus::Running);
                assert!(selection.is_none());
                assert!(plan.source_url.contains("example.com"));
            }
            other => panic!("expected plan, got {other:?}"),
        }
    }

    #[test]
    fn route_turn_downloads_existing_resources() {
        let panel = panel_with_resources();
        match route_turn("下载第 2 个", &panel, 100) {
            AgentTurn::Plan {
                plan,
                url,
                selection,
            } => {
                assert!(url.is_none());
                assert_eq!(
                    plan.intent,
                    AgentIntent::DownloadExisting(DownloadSelection::Index(2))
                );
                assert_eq!(selection, Some(DownloadSelection::Index(2)));
                assert_eq!(plan.steps[1].tool, AgentTool::Download);
                assert_eq!(plan.steps[1].status, StepStatus::Running);
            }
            other => panic!("expected plan, got {other:?}"),
        }
    }

    #[test]
    fn route_turn_keeps_a_selected_only_command_without_selection_as_chat() {
        let mut panel = panel_with_resources();
        for card in panel.resources.iter_mut() {
            card.selected = false;
        }
        match route_turn("下载所选", &panel, 100) {
            AgentTurn::Chat { prompt } => assert!(prompt.contains("下载所选")),
            other => panic!("expected chat, got {other:?}"),
        }
    }

    #[test]
    fn route_turn_chats_without_a_target() {
        let panel = AiPanelState::default();
        match route_turn("怎么设置限速？", &panel, 100) {
            AgentTurn::Chat { prompt } => {
                assert!(prompt.contains("怎么设置限速？"));
                assert!(prompt.contains("解析状态"));
            }
            other => panic!("expected chat, got {other:?}"),
        }
    }

    #[test]
    fn plan_finishes_steps_in_order() {
        let mut plan = AgentPlan::new(
            AgentIntent::ResolveAndDownload,
            "https://example.com/page",
            "下载",
            100,
        );
        plan.start_step(AgentTool::Resolve, 100);
        plan.finish_step(
            AgentTool::Resolve,
            StepStatus::Done,
            Some("2 个资源".into()),
            500,
        );
        plan.start_step(AgentTool::Download, 500);
        plan.finish_step(
            AgentTool::Download,
            StepStatus::Done,
            Some("已创建 2 个任务".into()),
            900,
        );
        plan.start_step(AgentTool::Summarize, 900);
        plan.complete(Some("汇报完成".to_string()), 1200);
        assert_eq!(plan.state, AgentPlanState::Completed);
        assert!(!plan.is_running());
        assert_eq!(plan.progress_label(), "4/4");
        assert_eq!(plan.elapsed_ms(9999), 1100);
        assert_eq!(
            plan.step(AgentTool::Resolve).unwrap().status,
            StepStatus::Done
        );
        assert_eq!(
            plan.step(AgentTool::Summarize).unwrap().status,
            StepStatus::Done
        );
    }

    #[test]
    fn plan_failure_skips_pending_and_marks_running() {
        let mut plan = AgentPlan::new(
            AgentIntent::ResolveAndDownload,
            "https://example.com/page",
            "下载",
            100,
        );
        plan.start_step(AgentTool::Resolve, 100);
        plan.fail("抓取页面失败", 300);
        assert_eq!(plan.state, AgentPlanState::Failed);
        assert_eq!(
            plan.step(AgentTool::Resolve).unwrap().status,
            StepStatus::Failed
        );
        assert_eq!(
            plan.step(AgentTool::Download).unwrap().status,
            StepStatus::Skipped
        );
        assert_eq!(
            plan.step(AgentTool::Summarize).unwrap().status,
            StepStatus::Skipped
        );
        assert!(plan.summary.unwrap().contains("抓取页面失败"));
    }

    #[test]
    fn plan_cancel_marks_remaining_steps_skipped() {
        let mut plan = AgentPlan::new(AgentIntent::Resolve, "https://x.example", "解析", 0);
        plan.cancel("用户取消", 10);
        assert_eq!(plan.state, AgentPlanState::Cancelled);
        assert_eq!(
            plan.step(AgentTool::Resolve).unwrap().status,
            StepStatus::Skipped
        );
        assert_eq!(
            plan.step(AgentTool::Summarize).unwrap().status,
            StepStatus::Skipped
        );
    }

    #[test]
    fn plan_step_duration_tracks_running_and_finished() {
        let mut plan = AgentPlan::new(AgentIntent::Resolve, "https://x.example", "解析", 100);
        let step = plan.step(AgentTool::Resolve).unwrap();
        assert_eq!(step.duration_ms(250), Some(150));
        plan.finish_step(AgentTool::Resolve, StepStatus::Done, None, 400);
        assert_eq!(
            plan.step(AgentTool::Resolve).unwrap().duration_ms(999),
            Some(300)
        );
    }

    #[test]
    fn selected_resource_indices_supports_all_modes() {
        let panel = panel_with_resources();
        assert_eq!(
            selected_resource_indices(&panel.resources, DownloadSelection::All),
            vec![0, 1]
        );
        assert_eq!(
            selected_resource_indices(&panel.resources, DownloadSelection::Selected),
            vec![0]
        );
        assert_eq!(
            selected_resource_indices(&panel.resources, DownloadSelection::Index(2)),
            vec![1]
        );
        assert_eq!(
            selected_resource_indices(&panel.resources, DownloadSelection::Index(9)),
            Vec::<usize>::new()
        );
        assert_eq!(
            selected_resource_indices(&panel.resources, DownloadSelection::Kind(KindFilter::Audio)),
            vec![1]
        );
    }

    #[test]
    fn resolve_summary_reports_resources_and_created_tasks() {
        let plan = AgentPlan::new(
            AgentIntent::ResolveAndDownload,
            "https://example.com/page",
            "下载",
            0,
        );
        let report = report(
            vec![("a.mp4", "video"), ("b.m4a", "audio")],
            &["页面 2 个资源"],
        );
        let created = vec!["a.mp4".to_string(), "b.m4a".to_string()];
        let summary = resolve_summary(&plan, &report, &created);
        assert!(summary.contains("已解析到 2 个资源"));
        assert!(summary.contains("视频"));
        assert!(summary.contains("已自动创建 2 个下载任务"));
        assert!(summary.contains("a.mp4"));
        assert!(!summary.contains("请手动"));
    }

    #[test]
    fn resolve_summary_without_auto_download_points_at_buttons() {
        let plan = AgentPlan::new(AgentIntent::Resolve, "https://example.com/page", "解析", 0);
        let report = report(vec![("a.mp4", "video")], &[]);
        let summary = resolve_summary(&plan, &report, &[]);
        assert!(summary.contains("下载全部"));
        assert!(!summary.contains("已自动创建"));
    }

    #[test]
    fn resolve_summary_explains_failure_without_resources() {
        let plan = AgentPlan::new(AgentIntent::Resolve, "https://example.com/page", "解析", 0);
        let report = report(vec![], &["抓取页面失败（HTTP 403）", "AI 未返回有效结果"]);
        let summary = resolve_summary(&plan, &report, &[]);
        assert!(summary.contains("没有解析到可下载资源"));
        assert!(summary.contains("HTTP 403"));
        assert!(summary.contains("重试解析"));
        assert!(summary.contains("登录"));
    }

    #[test]
    fn download_summary_reports_created_and_missing() {
        let summary = download_summary(
            DownloadSelection::All,
            "https://example.com/page",
            &["a.mp4".to_string()],
        );
        assert!(summary.contains("已创建 1 个下载任务"));
        assert!(summary.contains("example.com"));

        let missing = download_summary(DownloadSelection::Kind(KindFilter::Audio), "", &[]);
        assert!(missing.contains("没有找到音频资源"));
        let index = download_summary(DownloadSelection::Index(9), "", &[]);
        assert!(index.contains("第 9 个"));
    }

    #[test]
    fn summary_prompt_contains_facts_and_forbids_manual_tutorials() {
        let mut plan = AgentPlan::new(
            AgentIntent::ResolveAndDownload,
            "https://example.com/page",
            "帮我下载 https://example.com/page",
            0,
        );
        plan.finish_step(
            AgentTool::Resolve,
            StepStatus::Done,
            Some("2 个资源".into()),
            10,
        );
        plan.start_step(AgentTool::Download, 10);
        plan.finish_step(
            AgentTool::Download,
            StepStatus::Done,
            Some("已创建 2 个任务".into()),
            20,
        );
        plan.created_downloads = vec!["a.mp4".to_string(), "b.m4a".to_string()];
        let report = report(
            vec![("a.mp4", "video"), ("b.m4a", "audio")],
            &["页内发现 2 个媒体"],
        );
        let prompt = summary_prompt(
            "帮我下载 https://example.com/page?token=secret#frag",
            &plan,
            &report,
        );
        assert!(prompt.contains("已执行的工作流"));
        assert!(prompt.contains("已创建下载任务"));
        assert!(prompt.contains("a.mp4"));
        assert!(prompt.contains("页内发现 2 个媒体"));
        assert!(!prompt.contains("token=secret"), "{prompt}");
        assert!(!prompt.contains("https://"), "{prompt}");
        assert!(prompt.contains("[链接 example.com]"), "{prompt}");
        assert!(summary_system_prompt().contains("不要教用户手动"));
        assert_eq!(
            scrub_urls("下载 https://example.com/a?token=x"),
            "下载 [链接 example.com]"
        );
    }

    #[test]
    fn chat_prompt_scrubs_urls_and_only_shows_the_host() {
        let mut panel = panel_with_resources();
        panel.source_url = "https://example.com/page?token=secret".to_string();
        let prompt = chat_prompt("这个 https://example.com/a?x=1 怎么下载？", &panel);
        assert!(!prompt.contains("token=secret"), "{prompt}");
        assert!(!prompt.contains("?x=1"), "{prompt}");
        assert!(prompt.contains("[链接 example.com]"), "{prompt}");
        assert!(prompt.contains("已解析来源：example.com"), "{prompt}");
    }

    #[test]
    fn pretty_host_never_contains_query() {
        assert_eq!(
            pretty_host("https://WWW.Example.COM/path?token=secret#x"),
            "www.example.com"
        );
        assert_eq!(pretty_host("not a url"), "not a url");
        assert!(pretty_host("").is_empty());
    }

    #[test]
    fn resource_kind_labels_are_chinese() {
        assert_eq!(resource_kind_label("video"), "视频");
        assert_eq!(resource_kind_label("audio"), "音频");
        assert_eq!(resource_kind_label("unknown"), "其它");
    }
}
