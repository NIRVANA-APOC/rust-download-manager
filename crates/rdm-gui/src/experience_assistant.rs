//! 解析经验 AI 助手（design 18 §4）：prompt 构造 + 回复解析 + 新版本提案。
//!
//! 纯函数模块（不依赖 GPUI/网络），由 `views/experience.rs` 的助手面板与 `views/app.rs`
//! 的事件接线共同使用。职责边界：
//!
//! - 把**当前选中经验**（完整 recipe JSON、scope/可复用性、样本 URL）作为上下文；
//! - 要求模型先给中文说明，再给一个 ` ```json ` 代码块（完整 `rdm.recipe/v1`）；
//! - 从回复中提取并校验最后一条 recipe JSON，强制当前经验 id / 下一版本号；
//! - 生成人类可读的“关键变化”摘要，供 UI 预览卡展示；绝不静默写盘。

use rdm_resolve::{Experience, Recipe, RECIPE_SCHEMA};

/// 助手面板的快捷动作（chips）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickAction {
    /// 抽象匹配范围/参数，产出可复用模板。
    MakeReusable,
    /// 修复流程中仍写死的内部参数（如 cid）。
    FixHardcoded,
    /// 只解释流程，不建议修改。
    Explain,
}

impl QuickAction {
    pub fn label_zh(self) -> &'static str {
        match self {
            Self::MakeReusable => "整理为可复用经验",
            Self::FixHardcoded => "修复写死参数",
            Self::Explain => "解释这个流程",
        }
    }

    /// 作为用户消息发送的完整指令（包含明确产出格式，避免模型自由发挥）。
    pub fn prompt(self) -> &'static str {
        match self {
            Self::MakeReusable => {
                "请把当前经验整理成可复用模板：match 只保留 host+静态路径前缀，\
                 把样本 URL 中的可变段（如 BV 号）抽象成命名捕获，并把步骤/输出中的\
                 样本值替换为 {{变量}} 模板；保持流程仍能解析样本 URL。\
                 先给 2-4 行中文说明，再给一个 ```json 代码块（完整 rdm.recipe/v1，含 tests）。"
            }
            Self::FixHardcoded => {
                "这个经验里仍有写死的内部参数（例如 cid）。请改造成动态获取：\
                 允许新增前置 http_request/json_query 步骤，用输入捕获的变量调用接口、\
                 再引用接口返回值，不要写死任何样本专属 id；保持流程可验证。\
                 先给 2-4 行中文说明，再给一个 ```json 代码块（完整 rdm.recipe/v1，含 tests）。"
            }
            Self::Explain => {
                "请用中文解释这个经验的匹配范围、每个步骤在做什么、输出资源是什么，\
                 以及它对哪些同类链接可复用、有哪些限制。不要输出 JSON，不要修改经验。"
            }
        }
    }
}

/// 一轮助手对话（用于把上下文/历史拼进 user prompt）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub from_user: bool,
    pub text: String,
}

impl TranscriptTurn {
    #[allow(dead_code)]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            from_user: true,
            text: text.into(),
        }
    }

    #[allow(dead_code)]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            from_user: false,
            text: text.into(),
        }
    }
}

/// 系统提示：角色 + 当前经验完整 JSON + 硬性输出约束。
pub fn system_prompt(experience: &Experience) -> String {
    let recipe_json =
        serde_json::to_string_pretty(&experience.recipe).unwrap_or_else(|_| "{}".to_owned());
    let sample_url = experience.source_url.as_deref().unwrap_or("（无）");
    let reusable = experience
        .recipe
        .generalization
        .as_ref()
        .map(|generalization| generalization.reusable)
        .unwrap_or(false);
    let leftovers = experience
        .recipe
        .generalization
        .as_ref()
        .map(|generalization| generalization.leftover_hardcoded.join("、"))
        .unwrap_or_default();
    format!(
        "你是 RDM 的解析经验维护助手。目标：把经验维护成**可复用模板**，而不是单条链接的历史记录。\n\
         当前经验 id：{id}，版本：v{version}，样本 URL：{sample_url}\n\
         当前是否可复用：{reusable}；仍写死参数：{leftovers}\n\
         \n当前经验 JSON：\n```json\n{recipe_json}\n```\n\
         \n规则：\n\
         1. match 只保留 host + 静态路径前缀；可变段用命名捕获（例如 BV 号 → (?P<bvid>BV[0-9A-Za-z]+)），\
            query/fragment 默认忽略；不要用样本值写死正则。\n\
         2. steps/emit 中所有样本相关值改成 {{{{变量}}}} 模板；需要内部 id（如 cid）时，\
            可以新增前置 http_request/json_query 步骤动态获取。\n\
         3. 输出顺序：先 2-4 行中文说明，然后恰好一个 ```json 代码块，内含完整 rdm.recipe/v1\
            （schema、id、version、match、captures、steps、emit、tests 齐全）。\n\
         4. 不得包含 cookie/authorization/header 值，不得编造未验证的新站点结构。\n\
         5. 若用户只是询问/解释，不要输出 JSON。",
        id = experience.id,
        version = experience.active_version,
        sample_url = sample_url,
        reusable = if reusable { "是" } else { "否" },
        leftovers = if leftovers.is_empty() {
            "无".to_owned()
        } else {
            leftovers
        },
    )
}

/// 拼装本次 user prompt：经验上下文之外带上最近对话（防止多轮失忆）。
pub fn build_prompt(history: &[TranscriptTurn], user_text: &str) -> String {
    if history.is_empty() {
        return user_text.trim().to_owned();
    }
    let mut out = String::from("（对话历史）\n");
    for turn in history {
        out.push_str(if turn.from_user {
            "用户："
        } else {
            "助手："
        });
        out.push_str(turn.text.trim());
        out.push('\n');
    }
    out.push_str("\n（本轮请求）\n");
    out.push_str(user_text.trim());
    out
}

/// 助手回复里最后一个合法 `rdm.recipe/v1` JSON 代码块（原样返回 JSON 文本）。
///
/// 只认 fenced block（```json ... ```），且必须能解析出 `schema == rdm.recipe/v1`；
/// 解析失败/不存在返回 `None`（UI 只显示普通回复，不出现“应用”按钮）。
pub fn extract_recipe_json(reply: &str) -> Option<String> {
    let mut found: Option<String> = None;
    let mut offset = 0usize;
    while let Some(start) = reply[offset..].find("```") {
        let after_fence = start + 3 + offset;
        let Some(info_end_rel) = reply[after_fence..].find('\n') else {
            break;
        };
        let info = &reply[after_fence..after_fence + info_end_rel];
        let content_start = after_fence + info_end_rel + 1;
        let Some(end_rel) = reply[content_start..].find("```") else {
            break;
        };
        let content = &reply[content_start..content_start + end_rel];
        let is_json_block = info.trim().is_empty()
            || info
                .trim()
                .split_ascii_whitespace()
                .any(|word| word.eq_ignore_ascii_case("json"));
        if is_json_block && looks_like_recipe(content) {
            found = Some(content.trim().to_owned());
        }
        offset = content_start + end_rel + 3;
    }
    found
}

fn looks_like_recipe(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return false;
    };
    value.get("schema").and_then(|schema| schema.as_str()) == Some(RECIPE_SCHEMA)
        && value.get("steps").is_some()
        && value.get("emit").is_some()
}

/// 待用户确认的“应用为新版本”提案。
#[derive(Debug, Clone)]
pub struct PatchProposal {
    /// 已强制 id/version 并校验通过的 recipe。
    pub recipe: Recipe,
    pub next_version: u32,
    /// 人类可读的关键变化（≤6 条，供预览卡展示）。
    pub changes: Vec<String>,
}

impl PatchProposal {
    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        self.changes.join("；")
    }
}

/// 从助手回复生成提案：提取 JSON → 校验 → 强制当前 id/下一版本 → 计算变化摘要。
///
/// 失败返回中文错误（可直接内联展示），绝不写盘。调用方负责在用户点“应用”后
/// 通过 `ExperienceStore::update` 落盘。
pub fn proposal(current: &Recipe, reply: &str) -> Result<PatchProposal, String> {
    let Some(json) = extract_recipe_json(reply) else {
        return Err("回复里没有可解析的 rdm.recipe/v1 JSON".to_owned());
    };
    let mut recipe =
        Recipe::from_json(&json).map_err(|error| format!("建议的 JSON 不合法：{error}"))?;
    recipe.id = current.id.clone();
    let Some(next_version) = current.version.checked_add(1) else {
        return Err("版本号已用尽".to_owned());
    };
    recipe.version = next_version;
    recipe.validate()?;
    let changes = summarize_changes(current, &recipe);
    Ok(PatchProposal {
        recipe,
        next_version,
        changes,
    })
}

/// 比较新旧 recipe，给出人类可读变化（UI 预览卡）。
pub fn summarize_changes(current: &Recipe, next: &Recipe) -> Vec<String> {
    let mut changes = Vec::new();
    if current.matcher.url_pattern != next.matcher.url_pattern {
        changes.push(format!(
            "匹配规则：{} → {}",
            current
                .matcher
                .url_pattern
                .as_deref()
                .unwrap_or("<任意路径>"),
            next.matcher.url_pattern.as_deref().unwrap_or("<任意路径>")
        ));
    }
    if current.captures != next.captures {
        if next.captures.is_empty() {
            changes.push("移除捕获变量".to_owned());
        } else {
            changes.push(format!("捕获变量：{}", next.captures.join("、")));
        }
    }
    if current.steps.len() != next.steps.len() {
        changes.push(format!(
            "步骤数：{} → {}",
            current.steps.len(),
            next.steps.len()
        ));
    } else if let Some(step) = first_changed_step(current, next) {
        changes.push(format!("步骤 {step} 有修改"));
    }
    if current.emit.url != next.emit.url
        || current.emit.name != next.emit.name
        || current.emit.kind != next.emit.kind
    {
        changes.push("输出资源有修改".to_owned());
    }
    if let (Some(before), Some(after)) = (&current.generalization, &next.generalization) {
        if before.reusable != after.reusable {
            changes.push(format!(
                "可复用性：{} → {}",
                yes_no(before.reusable),
                yes_no(after.reusable)
            ));
        }
        if before.leftover_hardcoded != after.leftover_hardcoded {
            changes.push(if after.leftover_hardcoded.is_empty() {
                "清除写死参数".to_owned()
            } else {
                format!("仍写死：{}", after.leftover_hardcoded.join("、"))
            });
        }
    }
    if changes.is_empty() {
        changes.push("内容无实质变化".to_owned());
    }
    changes.truncate(6);
    changes
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "可复用"
    } else {
        "仅精确地址"
    }
}

fn first_changed_step<'a>(current: &'a Recipe, next: &Recipe) -> Option<&'a str> {
    let by_id: std::collections::BTreeMap<&str, &rdm_resolve::RecipeStep> = next
        .steps
        .iter()
        .map(|step| (step.id.as_str(), step))
        .collect();
    current
        .steps
        .iter()
        .find(|step| {
            by_id
                .get(step.id.as_str())
                .is_some_and(|next_step| !step_semantically_equal(step, next_step))
        })
        .map(|step| step.id.as_str())
}

fn step_semantically_equal(
    left: &rdm_resolve::RecipeStep,
    right: &rdm_resolve::RecipeStep,
) -> bool {
    left.op == right.op
        && left.args == right.args
        && left.assign == right.assign
        && left.on_fail == right.on_fail
        && serde_json::to_value(&left.when).ok() == serde_json::to_value(&right.when).ok()
        && serde_json::to_value(&left.expect).ok() == serde_json::to_value(&right.expect).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE_JSON: &str = r#"{
        "schema": "rdm.recipe/v1",
        "id": "demo",
        "version": 1,
        "name": "",
        "match": {"hosts": ["example.com"], "url_pattern": "^https://example\\.com/v/(?P<id>[0-9]+)$"},
        "captures": ["id"],
        "steps": [
            {"id": "s0", "op": "http_request", "args": {"method": "GET", "url": "https://api.example.com/v/{{id}}"}}
        ],
        "emit": {"url": "https://cdn.example.com/{{id}}.mp4", "name": "{{id}}.mp4"},
        "tests": [{"name": "sample", "input_url": "https://example.com/v/42"}]
    }"#;

    fn recipe(reply: &str) -> Result<PatchProposal, String> {
        let current = Recipe::from_json(RECIPE_JSON).expect("current");
        proposal(&current, reply)
    }

    #[test]
    fn quick_actions_have_labels_and_prompts() {
        for action in [
            QuickAction::MakeReusable,
            QuickAction::FixHardcoded,
            QuickAction::Explain,
        ] {
            assert!(!action.label_zh().is_empty());
            assert!(!action.prompt().is_empty());
        }
    }

    #[test]
    fn extract_recipe_json_finds_last_valid_block() {
        let reply =
            "先说明。\n```json\n{\"schema\":\"rdm.recipe/v1\",\"steps\":[],\"emit\":{}}\n```\n\
                     继续。\n```json\n{\"schema\":\"other\",\"steps\":[]}\n```";
        // 第一个块缺 emit 之外的必填字段但 looks_like 只做结构粗检；这里断言能提取到它。
        let extracted = extract_recipe_json(reply).expect("block");
        assert!(extracted.contains("rdm.recipe/v1"));
        assert!(extract_recipe_json("没有代码块").is_none());
        assert!(extract_recipe_json("```json\n{}\n```").is_none());
    }

    #[test]
    fn proposal_forces_id_and_next_version() {
        let mut next: serde_json::Value = serde_json::from_str(RECIPE_JSON).unwrap();
        next["id"] = "attacker".into();
        next["version"] = 99.into();
        next["name"] = "更可复用".into();
        let reply = format!(
            "说明文字\n```json\n{}\n```",
            serde_json::to_string_pretty(&next).unwrap()
        );
        let patch = recipe(&reply).expect("proposal");
        assert_eq!(patch.recipe.id, "demo");
        assert_eq!(patch.recipe.version, 2);
        assert_eq!(patch.next_version, 2);
        assert!(!patch.changes.is_empty());
    }

    #[test]
    fn proposal_rejects_invalid_json() {
        let error = recipe("```json\n{\"schema\":\"rdm.recipe/v1\",\"steps\":[],\"emit\":{}}\n```")
            .unwrap_err();
        assert!(error.contains("不合法"), "{error}");
        let missing = recipe("只有说明").unwrap_err();
        assert!(missing.contains("没有可解析"), "{missing}");
    }

    #[test]
    fn build_prompt_includes_history_and_request() {
        let prompt = build_prompt(
            &[
                TranscriptTurn::user("整理一下"),
                TranscriptTurn::assistant("收到"),
            ],
            "修复 cid",
        );
        assert!(prompt.contains("用户：整理一下"));
        assert!(prompt.contains("助手：收到"));
        assert!(prompt.ends_with("修复 cid"));
        assert_eq!(build_prompt(&[], " 仅本轮 "), "仅本轮");
    }

    #[test]
    fn summarize_reports_scope_steps_and_emit_changes() {
        let current = Recipe::from_json(RECIPE_JSON).expect("current");
        let mut next = current.clone();
        next.matcher.url_pattern = Some("^https://example\\.com/".to_owned());
        next.captures.clear();
        next.steps.push(rdm_resolve::RecipeStep {
            id: "s1".to_owned(),
            op: "json_query".to_owned(),
            args: serde_json::json!({"from": "o0", "pointer": "/url"}),
            expect: None,
            assign: Default::default(),
            on_fail: None,
            when: None,
        });
        next.emit.url = "{{url}}".to_owned();
        let changes = summarize_changes(&current, &next);
        assert!(changes.iter().any(|change| change.contains("匹配规则")));
        assert!(changes.iter().any(|change| change.contains("步骤数")));
        assert!(changes.iter().any(|change| change.contains("输出资源")));
    }
}
