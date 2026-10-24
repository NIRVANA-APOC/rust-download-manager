//! 解析经验页 v2（`docs/design/18-experience-v2-reusable.md` §3.1/§3.2）。
//!
//! 经验是用户拥有的「能力资产」：本页按 **三栏** 组织
//! `[列表 300] | [详情 flex] | [经验助手 340]`，展示
//! [`rdm_resolve::experience::Experience`]（可复用规则、主流程 + 补充流程、同类合并提示），
//! 并提供「使用此经验解析」（在新地址上回放，不调用 AI）、编辑（保存为新版本）与删除。
//!
//! 与 `views/monitor.rs` 相同的分层：
//! - [`ExperiencePageState`] 持有只读数据、搜索/编辑/URL 输入框、经验助手消息与模态开关；
//! - [`ExperienceCallbacks`] 由 `RdmApp` 用 `cx.listener` 构造，页面不直接访问 app 私有字段；
//! - [`render_experience_page`] 是纯渲染函数，颜色只来自 [`ThemeTokens`]。
//!
//! 助手面板只负责 **UI 状态机 + 消息上屏**：AI 请求由 `RdmApp` 发起，流式回调经
//! [`ExperiencePageState::assistant_begin`] / [`ExperiencePageState::assistant_delta`] /
//! [`ExperiencePageState::assistant_finished`] / [`ExperiencePageState::assistant_failed`]
//! 驱动；AI 回复里的 recipe JSON 由 [`crate::experience_assistant::proposal`] 转成
//! [`PatchProposal`] 预览卡，用户点「应用为新版本」后由 `RdmApp` 调
//! [`ExperiencePageState::take_patch_recipe`] 落盘（校验失败走 [`ExperiencePageState::set_patch_error`]）。
//!
//! 双模态（编辑 / 删除）用页内覆盖层实现（`occlude()`），不依赖 window dialog；
//! Esc 由覆盖层自己的 `on_key_down` 关闭。编辑/搜索/URL/助手输入框的文本镜像由
//! `InputEvent::Change` 订阅同步到 `Rc<RefCell>`，因此状态方法不需要 `cx` 就能读到最新输入。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, FocusHandle, FontWeight,
    Hsla, IntoElement, KeyDownEvent, ScrollHandle, SharedString, Styled, Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, Disableable, Sizable};
use rdm_resolve::experience::{
    Experience, ExperienceFlow, ExperienceStore, ExperienceVersion, FlowStep,
};
use rdm_resolve::health::{MethodStatus, RecipeHealth};
use rdm_resolve::recipe::{Recipe, RecipeTest};

use crate::experience_assistant::{self, PatchProposal, QuickAction};
use crate::model::{format_bytes, now_ms};
use crate::ui::icons::RdmIcon;
use crate::ui::theme::{
    self, ThemeTokens, FS_BODY, FS_CAPTION, FS_H1, FS_MICRO, FS_MONO_MICRO, RADIUS_LG, RADIUS_SM,
    RADIUS_XS,
};
use crate::ui::widgets::{
    divider, empty_state, info_banner, mono_text, soft_tag, status_pill, Severity,
};
use crate::views::app::RdmApp;

// ---------------------------------------------------------------------------
// 布局常量（design 18 §3.1）
// ---------------------------------------------------------------------------

/// 左侧经验列表宽度（design 18 §3.1：300px）。
const LIST_W: f32 = 300.0;
/// 右侧经验助手宽度（design 18 §3.1：340px）。
const ASSISTANT_W: f32 = 340.0;
/// 助手折叠后右侧竖条按钮宽度。
const ASSISTANT_RAIL_W: f32 = 44.0;
/// 窄窗断点：窗口宽度 < 1180 时助手默认折叠为按钮。
///
/// 默认窗口（1281 逻辑 px @125% DPI）必须默认展开助手（用户要求「经验助手在侧边栏」）；
/// 1180–1300 区间三栏仍可读（列表 300 + 助手 340，详情 ≥540px）。
const ASSISTANT_BREAKPOINT: f32 = 1180.0;
/// 页面内边距（design 17 §3.2：16px）。
const PAGE_PAD: f32 = 16.0;
/// 详情块之间的间距。
const SECTION_GAP: f32 = 12.0;
/// 编辑/删除模态层宽度。
const EDIT_MODAL_W: f32 = 620.0;
const DELETE_MODAL_W: f32 = 440.0;
/// 编辑表单滚动区最大高度。
const EDIT_FORM_MAX_H: f32 = 520.0;
/// 助手消息最多保留条数（超出丢弃最早的系统提示/消息）。
const ASSISTANT_MESSAGES_MAX: usize = 60;
/// 单条助手消息最多字符数（防止模型/流式异常把内存打爆）。
const ASSISTANT_MESSAGE_CHARS_MAX: usize = 60_000;

// ---------------------------------------------------------------------------
// 页面状态
// ---------------------------------------------------------------------------

/// 搜索输入 + 编辑表单的镜像文本。
///
/// 输入框是 GPUI 的 [`Entity<InputState>`]，本类型保存它们的**当前文本**；订阅会在
/// `InputEvent::Change` 时同步，因此页面状态的方法不需要 `cx` 就能读到最新值。
#[derive(Debug, Clone, Default)]
struct EditFormValues {
    name: String,
    description: String,
    hosts: String,
    url_pattern: String,
    priority: String,
    json: String,
}

/// 正在编辑的经验。
struct EditState {
    /// 被编辑经验的 id（保存时强制回填到 recipe）。
    id: String,
    /// 编辑基线版本（保存时新版本号 = base + 1）。
    base_version: u32,
    /// 行内校验/保存错误。
    error: Option<String>,
}

/// 等待二次确认的删除目标。
struct DeleteState {
    id: String,
    display_name: String,
}

/// 编辑表单的输入框集合（页面生命周期内复用，打开编辑时回填）。
struct EditInputs {
    name: Entity<InputState>,
    description: Entity<InputState>,
    hosts: Entity<InputState>,
    url_pattern: Entity<InputState>,
    priority: Entity<InputState>,
    json: Entity<InputState>,
}

impl EditInputs {
    fn new(window: &mut Window, cx: &mut Context<RdmApp>) -> Self {
        let name =
            cx.new(|cx| InputState::new(window, cx).placeholder("例如：示例站直链（可留空）"));
        let description = cx.new(|cx| {
            InputState::new(window, cx).placeholder("这条经验适用于什么页面/资源（可留空）")
        });
        let hosts = cx.new(|cx| {
            InputState::new(window, cx).placeholder("example.com, *.cdn.example.com（逗号分隔）")
        });
        let url_pattern = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("留空 = 该主机任意路径；例如 ^https://example\\.com/d/(\\d+)$")
        });
        let priority = cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        let json = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(6, 16)
                .soft_wrap(false)
                .placeholder("rdm.recipe/v1 流程 JSON")
        });
        Self {
            name,
            description,
            hosts,
            url_pattern,
            priority,
            json,
        }
    }

    /// 打开编辑时把一条 recipe 的字段回填到输入框。
    fn fill(&self, values: &EditFormValues, window: &mut Window, cx: &mut Context<RdmApp>) {
        let pairs = [
            (self.name.clone(), values.name.clone()),
            (self.description.clone(), values.description.clone()),
            (self.hosts.clone(), values.hosts.clone()),
            (self.url_pattern.clone(), values.url_pattern.clone()),
            (self.priority.clone(), values.priority.clone()),
            (self.json.clone(), values.json.clone()),
        ];
        for (input, value) in pairs {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
    }
}

/// 经验助手消息角色（design 18 §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantRole {
    User,
    Assistant,
    /// 本地系统提示（错误、应用失败、提示先选经验等）。
    System,
}

/// 经验助手的一轮消息；流式输出时最后一条 `Assistant` 的 `streaming = true`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantMessage {
    pub role: AssistantRole,
    pub text: String,
    pub streaming: bool,
}

impl AssistantMessage {
    fn user(text: impl Into<String>) -> Self {
        Self {
            role: AssistantRole::User,
            text: text.into(),
            streaming: false,
        }
    }

    fn assistant_streaming() -> Self {
        Self {
            role: AssistantRole::Assistant,
            text: String::new(),
            streaming: true,
        }
    }

    fn system(text: impl Into<String>) -> Self {
        Self {
            role: AssistantRole::System,
            text: text.into(),
            streaming: false,
        }
    }

    /// 空占位也显示（「正在思考…」），否则会像消息丢失。
    fn is_visible(&self) -> bool {
        self.streaming || !self.text.trim().is_empty()
    }
}

/// 解析经验页的只读状态 + 页内交互状态（由 `RdmApp` 持有）。
pub struct ExperiencePageState {
    store: ExperienceStore,
    experiences: Vec<Experience>,
    selected_id: Option<String>,
    /// 正在预览的历史版本；`None` = 跟随 active 版本。
    selected_version: Option<u32>,
    search_input: Entity<InputState>,
    search_query: Rc<RefCell<String>>,
    edit_inputs: EditInputs,
    edit_mirror: Rc<RefCell<EditFormValues>>,
    edit: Option<EditState>,
    delete: Option<DeleteState>,
    /// 编辑/删除模态层的键盘焦点（打开模态时聚焦，Esc 关闭）。
    modal_focus: FocusHandle,
    loaded_ms: u64,
    // ---- 「使用此经验解析」（design 18 §3.1） -------------------------
    /// 单行 URL 输入（点「使用此经验解析」后在详情顶部展开）。
    use_input: Entity<InputState>,
    /// URL 输入框镜像文本。
    use_url: Rc<RefCell<String>>,
    /// 正在为哪条经验展开 URL 输入；`None` = 未展开。
    use_experience_id: Option<String>,
    /// URL 输入的行内校验错误（由 `RdmApp` 回放失败时写入）。
    use_error: Option<String>,
    // ---- 同类合并提示（design 18 §3.1） -------------------------------
    /// 点了「稍后」的同类分组键；换组/刷新后重新提示。
    merge_dismissed: Option<String>,
    // ---- 经验助手（design 18 §3.2） -----------------------------------
    /// 助手输入框（多行；Enter 发送、Shift+Enter 换行由 `RdmApp` 订阅接线）。
    assistant_input: Entity<InputState>,
    /// 助手输入框镜像文本。
    assistant_draft: Rc<RefCell<String>>,
    assistant_messages: Vec<AssistantMessage>,
    assistant_busy: bool,
    /// AI 回复里可应用的 recipe 提案（预览卡数据源）。
    assistant_patch: Option<PatchProposal>,
    /// 助手行内错误（JSON 不合法 / 应用新版本失败）。
    assistant_error: Option<String>,
    assistant_scroll: ScrollHandle,
    /// 宽窗用户偏好：`true` = 展开助手。
    assistant_open: bool,
    /// 窄窗下用户单独点开过助手（窄窗默认折叠为按钮）。
    assistant_narrow_open: bool,
    /// 最近一帧助手是否实际可见（toggle 时据此取反）。
    assistant_last_visible: Cell<bool>,
    _subscriptions: Vec<Subscription>,
}

impl ExperiencePageState {
    /// 构造页面状态；首次读取 `rdm_ipc::state_dir()/resolve`。
    ///
    /// `state_dir` 以 `RdmApp` 为准：建议构造后立刻调用
    /// [`Self::reload`] 传入 app 的真实状态目录（两者正常情况下一致）。
    pub fn new(window: &mut Window, cx: &mut Context<RdmApp>) -> Self {
        let store = ExperienceStore::from_state_dir(&rdm_ipc::state_dir());
        let experiences = store.list();

        let search_query = Rc::new(RefCell::new(String::new()));
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索名称、主机或 URL…"));

        let use_url = Rc::new(RefCell::new(String::new()));
        let use_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("粘贴同类新链接，例如 https://www.example.com/video/BV…")
                .clean_on_escape()
        });

        let assistant_draft = Rc::new(RefCell::new(String::new()));
        let assistant_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(2, 6)
                .placeholder("描述要如何整理/修复这条经验…（Enter 发送，Shift+Enter 换行）")
        });

        let edit_mirror = Rc::new(RefCell::new(EditFormValues::default()));
        let edit_inputs = EditInputs::new(window, cx);
        let modal_focus = cx.focus_handle().tab_stop(true);

        let mut subscriptions = Vec::new();
        {
            let query = search_query.clone();
            subscriptions.push(cx.subscribe_in(
                &search_input,
                window,
                move |_this: &mut RdmApp,
                      input: &Entity<InputState>,
                      event: &InputEvent,
                      _window: &mut Window,
                      cx: &mut Context<RdmApp>| {
                    if matches!(event, InputEvent::Change) {
                        *query.borrow_mut() = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ));
        }
        subscriptions.push(subscribe_text_mirror(&use_input, window, cx, &use_url));
        subscriptions.push(subscribe_text_mirror(
            &assistant_input,
            window,
            cx,
            &assistant_draft,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.name,
            window,
            cx,
            &edit_mirror,
            EditField::Name,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.description,
            window,
            cx,
            &edit_mirror,
            EditField::Description,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.hosts,
            window,
            cx,
            &edit_mirror,
            EditField::Hosts,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.url_pattern,
            window,
            cx,
            &edit_mirror,
            EditField::UrlPattern,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.priority,
            window,
            cx,
            &edit_mirror,
            EditField::Priority,
        ));
        subscriptions.push(subscribe_edit_field(
            &edit_inputs.json,
            window,
            cx,
            &edit_mirror,
            EditField::Json,
        ));

        let mut state = Self {
            store,
            experiences,
            selected_id: None,
            selected_version: None,
            search_input,
            search_query,
            edit_inputs,
            edit_mirror,
            edit: None,
            delete: None,
            modal_focus,
            loaded_ms: now_ms(),
            use_input,
            use_url,
            use_experience_id: None,
            use_error: None,
            merge_dismissed: None,
            assistant_input,
            assistant_draft,
            assistant_messages: Vec::new(),
            assistant_busy: false,
            assistant_patch: None,
            assistant_error: None,
            assistant_scroll: ScrollHandle::new(),
            assistant_open: true,
            assistant_narrow_open: false,
            assistant_last_visible: Cell::new(true),
            _subscriptions: subscriptions,
        };
        state.select_first_if_any();
        state
    }

    /// 重新读盘（顶部刷新 / AI 解析完成后由 `RdmApp` 调用）。
    ///
    /// 编辑态只在目标经验仍存在时保留，避免表单指向已删除的经验。
    pub fn reload(&mut self, state_dir: &Path) {
        self.store = ExperienceStore::from_state_dir(state_dir);
        self.experiences = self.store.list();
        self.loaded_ms = now_ms();

        if let Some(selected) = self.selected_id.clone() {
            if !self.experiences.iter().any(|exp| exp.id == selected) {
                self.selected_id = self.experiences.first().map(|exp| exp.id.clone());
            }
        } else {
            self.selected_id = self.experiences.first().map(|exp| exp.id.clone());
        }
        let version_still_exists = match (self.selected_id.as_deref(), self.selected_version) {
            (Some(id), Some(version)) => self
                .experiences
                .iter()
                .find(|exp| exp.id == id)
                .is_some_and(|exp| exp.versions.iter().any(|item| item.version == version)),
            _ => true,
        };
        if !version_still_exists {
            self.selected_version = None;
        }
        let edit_missing = self
            .edit
            .as_ref()
            .is_some_and(|edit| !self.experiences.iter().any(|exp| exp.id == edit.id));
        if edit_missing {
            self.edit = None;
        }
        let delete_missing = self
            .delete
            .as_ref()
            .is_some_and(|target| !self.experiences.iter().any(|exp| exp.id == target.id));
        if delete_missing {
            self.delete = None;
        }
    }

    /// 当前选中的经验（详情面板数据源）。
    pub fn selected(&self) -> Option<&Experience> {
        let id = self.selected_id.as_deref()?;
        self.experiences.iter().find(|exp| exp.id == id)
    }

    /// 当前选中的经验 id。
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// 正在预览的版本号；`None` = 跟随 active 版本。
    #[allow(dead_code)] // Lead 接线保留的只读访问器
    pub fn selected_version(&self) -> Option<u32> {
        self.selected_version
    }

    /// 经验总数（侧栏计数 / 页面标题）。
    pub fn len(&self) -> usize {
        self.experiences.len()
    }

    /// 是否没有任何经验。
    #[allow(dead_code)] // Lead 接线保留的只读访问器
    pub fn is_empty(&self) -> bool {
        self.experiences.is_empty()
    }

    /// 重新读盘的时间（页面页头「刷新于 …」）。
    pub fn loaded_ms(&self) -> u64 {
        self.loaded_ms
    }

    /// 只读 store（`RdmApp` 保存/删除/回滚版本时使用）。
    pub fn store(&self) -> &ExperienceStore {
        &self.store
    }

    /// 当前搜索词（空串 = 不过滤）。
    #[allow(dead_code)] // Lead 接线保留的只读访问器
    pub fn search_query(&self) -> String {
        self.search_query.borrow().clone()
    }

    /// 按搜索词过滤后的经验列表（保持 store 的排序）。
    pub fn visible(&self) -> Vec<&Experience> {
        let query = self.search_query.borrow();
        if query.trim().is_empty() {
            return self.experiences.iter().collect();
        }
        self.experiences
            .iter()
            .filter(|exp| experience_matches(exp, query.as_str()))
            .collect()
    }

    /// 选中一条经验并回到 active 版本。
    ///
    /// design 18 §3.2：助手对话不跨经验复用（避免把 A 的回复应用成 B 的新版本），
    /// 切换经验时清空助手消息/提案。
    pub fn select(&mut self, id: &str) {
        if self.selected_id.as_deref() != Some(id) {
            self.reset_assistant();
            self.cancel_use_experience();
            self.merge_dismissed = None;
        }
        self.selected_id = Some(id.to_owned());
        self.selected_version = None;
    }

    /// 选中一条经验并预览指定版本（版本历史行点击）。
    pub fn preview_version(&mut self, id: &str, version: u32) {
        if self.selected_id.as_deref() != Some(id) {
            self.reset_assistant();
            self.cancel_use_experience();
            self.merge_dismissed = None;
        }
        self.selected_id = Some(id.to_owned());
        self.selected_version = Some(version);
    }

    fn select_first_if_any(&mut self) {
        self.selected_id = self.experiences.first().map(|exp| exp.id.clone());
        self.selected_version = None;
    }

    /// 详情面板当前展示的版本（预览版本，否则 active 版本）。
    fn viewed_version<'a>(&self, experience: &'a Experience) -> Option<&'a ExperienceVersion> {
        let version = self.selected_version?;
        experience
            .versions
            .iter()
            .find(|item| item.version == version)
    }

    /// 打开编辑模态层：回填 active 版本 recipe、聚焦模态层。
    pub fn begin_edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<RdmApp>) -> bool {
        let Some((recipe, base_version)) = self
            .experiences
            .iter()
            .find(|exp| exp.id == id)
            .map(|exp| (exp.recipe.clone(), exp.active_version))
        else {
            return false;
        };
        let values = EditFormValues {
            name: recipe.name.clone(),
            description: recipe.description.clone(),
            hosts: recipe.matcher.hosts.join(", "),
            url_pattern: recipe.matcher.url_pattern.clone().unwrap_or_default(),
            priority: recipe.matcher.priority.to_string(),
            json: serde_json::to_string_pretty(&recipe).unwrap_or_else(|_| "{}".to_owned()),
        };
        *self.edit_mirror.borrow_mut() = values.clone();
        self.edit_inputs.fill(&values, window, cx);
        self.edit = Some(EditState {
            id: id.to_owned(),
            base_version,
            error: None,
        });
        self.delete = None;
        window.focus(&self.modal_focus);
        true
    }

    /// 关闭编辑模态层（取消 / 保存成功后由 `RdmApp` 调用）。
    pub fn close_edit(&mut self) {
        self.edit = None;
    }

    /// 编辑表单的流程 JSON 当前文本（模态层未打开时为 `None`）。
    #[allow(dead_code)] // Lead 接线保留的只读访问器
    pub fn edit_json_text(&self) -> Option<String> {
        self.edit.as_ref()?;
        Some(self.edit_mirror.borrow().json.clone())
    }

    /// 写入行内错误（`RdmApp` 保存失败时调用）。
    pub fn set_edit_error(&mut self, error: Option<String>) {
        if let Some(edit) = self.edit.as_mut() {
            edit.error = error;
        }
    }

    /// 编辑表单的行内错误（渲染/测试用）。
    #[allow(dead_code)] // Lead 接线保留的只读访问器
    pub fn edit_error(&self) -> Option<&str> {
        self.edit.as_ref().and_then(|edit| edit.error.as_deref())
    }

    /// 把「表单字段 + 多行流程 JSON」合成为一条待保存的 recipe。
    ///
    /// 语义：JSON 是 active 版本流程的完整 recipe；名称/说明/适用主机/URL 正则/优先级
    /// 覆盖其中的 matcher 元数据；`id` 强制为被编辑经验 id，版本号强制为
    /// `base_version + 1`（`ExperienceStore::update` 会再把它作为新版本发布）。
    /// 解析/校验失败时把中文错误同时写进行内错误并返回 `Err`。
    pub fn edit_form_to_recipe(&mut self) -> Result<Recipe, String> {
        let Some(edit) = self.edit.as_ref() else {
            return Err("没有正在编辑的经验".to_owned());
        };
        let forced_id = edit.id.clone();
        let forced_version = edit.base_version.saturating_add(1).max(1);
        let values = self.edit_mirror.borrow().clone();
        let result = recipe_from_edit_json(&values.json, &values, &forced_id, forced_version);
        match &result {
            Ok(_) => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.error = None;
                }
            }
            Err(message) => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.error = Some(message.clone());
                }
            }
        }
        result
    }

    /// 打开删除确认模态层。
    pub fn begin_delete(
        &mut self,
        id: &str,
        window: &mut Window,
        _cx: &mut Context<RdmApp>,
    ) -> bool {
        let Some(display_name) = self
            .experiences
            .iter()
            .find(|exp| exp.id == id)
            .map(|exp| exp.display_name())
        else {
            return false;
        };
        self.delete = Some(DeleteState {
            id: id.to_owned(),
            display_name,
        });
        self.edit = None;
        window.focus(&self.modal_focus);
        true
    }

    /// 正在等待确认删除的经验 id。
    pub fn delete_target(&self) -> Option<&str> {
        self.delete.as_ref().map(|target| target.id.as_str())
    }

    /// 取消删除确认。
    pub fn cancel_delete(&mut self) {
        self.delete = None;
    }

    /// 是否有页内模态层（编辑 / 删除确认）打开（`RdmApp` 的 Esc 层级处理用）。
    pub fn modal_open(&self) -> bool {
        self.edit.is_some() || self.delete.is_some()
    }

    /// Esc：关闭页内模态层；返回 `true` 表示本次 Esc 被模态层消费。
    pub fn close_modal(&mut self) -> bool {
        if self.edit.is_some() {
            self.edit = None;
            true
        } else if self.delete.is_some() {
            self.delete = None;
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // 「使用此经验解析」（design 18 §3.1）
    // -----------------------------------------------------------------------

    /// 展开详情顶部的单行 URL 输入，并预填该经验的样本地址（有则预填）。
    pub fn begin_use_experience(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<RdmApp>,
    ) -> bool {
        let Some(prefill) = self
            .experiences
            .iter()
            .find(|exp| exp.id == id)
            .map(experience_sample_url)
        else {
            return false;
        };
        let prefill = prefill.unwrap_or_default();
        self.use_experience_id = Some(id.to_owned());
        self.use_error = None;
        *self.use_url.borrow_mut() = prefill.clone();
        self.use_input
            .update(cx, |state, cx| state.set_value(prefill, window, cx));
        self.use_input
            .update(cx, |state, cx| state.focus(window, cx));
        true
    }

    /// 收起 URL 输入（取消 / 回放结束后由 `RdmApp` 调用）。
    pub fn cancel_use_experience(&mut self) {
        self.use_experience_id = None;
        self.use_error = None;
    }

    /// 当前展开 URL 输入的经验 id。
    pub fn use_experience_target(&self) -> Option<&str> {
        self.use_experience_id.as_deref()
    }

    /// 当前 URL 输入文本（未展开时为空串）。
    pub fn use_url_text(&self) -> String {
        self.use_url.borrow().clone()
    }

    /// 读取 URL 输入文本（trim 后；空串 = 用户没输入，调用方负责校验/提示）。
    pub fn take_use_url(&mut self) -> String {
        self.use_url.borrow().trim().to_owned()
    }

    /// 写入 URL 输入的行内错误（回放失败等）。
    pub fn set_use_error(&mut self, error: Option<String>) {
        self.use_error = error;
    }

    /// 正在展示的 URL 行内错误。
    #[allow(dead_code)] // 供渲染与测试读取
    pub fn use_error(&self) -> Option<&str> {
        self.use_error.as_deref()
    }

    /// URL 输入框（`RdmApp` 订阅 Enter 用）。
    pub fn use_experience_input(&self) -> &Entity<InputState> {
        &self.use_input
    }

    // -----------------------------------------------------------------------
    // 同类合并提示（design 18 §3.1）
    // -----------------------------------------------------------------------

    /// 合并提示「稍后」：记住当前同类分组键，换组/刷新后重新提示。
    pub fn dismiss_merge_hint(&mut self) {
        self.merge_dismissed = self.selected().map(merge_scope_key);
    }

    /// 当前经验的合并提示是否已被「稍后」隐藏。
    pub fn merge_hint_dismissed(&self) -> bool {
        match (
            self.selected().map(merge_scope_key),
            self.merge_dismissed.as_ref(),
        ) {
            (Some(key), Some(dismissed)) => &key == dismissed,
            _ => false,
        }
    }

    /// 当前选中经验的合并计划（`related` + 自身，目标 = 最早创建的经验）。
    pub fn related_merge_plan(&self) -> Option<MergePlan> {
        let selected = self.selected()?;
        if selected.related.is_empty() {
            return None;
        }
        choose_merge_plan(selected, &self.experiences)
    }

    // -----------------------------------------------------------------------
    // 经验助手（design 18 §3.2）
    // -----------------------------------------------------------------------

    /// 助手输入框（`RdmApp` 订阅 Enter / Shift+Enter 用）。
    pub fn assistant_input(&self) -> &Entity<InputState> {
        &self.assistant_input
    }

    /// 助手输入框当前文本（发送按钮的入参）。
    pub fn assistant_draft(&self) -> String {
        self.assistant_draft.borrow().clone()
    }

    /// 助手消息流（用户 / AI / 系统提示）。
    pub fn assistant_messages(&self) -> &[AssistantMessage] {
        &self.assistant_messages
    }

    /// 助手是否正在等待/接收 AI 回复（发送按钮变停止、chips 禁用）。
    pub fn assistant_busy(&self) -> bool {
        self.assistant_busy
    }

    /// 待应用的「新版本」预览（渲染预览卡）。
    pub fn patch_preview(&self) -> Option<&PatchProposal> {
        self.assistant_patch.as_ref()
    }

    /// 助手/应用新版本的行内错误。
    pub fn assistant_patch_error(&self) -> Option<&str> {
        self.assistant_error.as_deref()
    }

    /// 读取并清空助手输入框（Enter 提交 / 发送按钮用）；trim 后为空返回空串。
    pub fn take_assistant_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<RdmApp>,
    ) -> String {
        let text = self.assistant_draft.borrow().trim().to_owned();
        self.assistant_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        *self.assistant_draft.borrow_mut() = String::new();
        text
    }

    /// 用户消息上屏 + 流式占位 + 置 busy；`RdmApp` 随后启动 AI 请求。
    pub fn assistant_begin(&mut self, user_text: &str) {
        let text = user_text.trim();
        if text.is_empty() {
            return;
        }
        self.assistant_error = None;
        self.assistant_patch = None;
        self.assistant_busy = true;
        push_assistant_message(&mut self.assistant_messages, AssistantMessage::user(text));
        push_assistant_message(
            &mut self.assistant_messages,
            AssistantMessage::assistant_streaming(),
        );
        self.assistant_scroll.scroll_to_bottom();
    }

    /// 流式增量：追加到最后一条 Assistant 占位（没有占位时补一条）。
    pub fn assistant_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let has_placeholder = matches!(
            self.assistant_messages.last(),
            Some(message) if message.role == AssistantRole::Assistant && message.streaming
        );
        if !has_placeholder {
            push_assistant_message(
                &mut self.assistant_messages,
                AssistantMessage::assistant_streaming(),
            );
        }
        if let Some(message) = self.assistant_messages.last_mut() {
            message.text = append_assistant_text(&message.text, delta, ASSISTANT_MESSAGE_CHARS_MAX);
        }
        self.assistant_scroll.scroll_to_bottom();
    }

    /// 一轮 AI 回复完成：定稿消息 → 尝试生成「新版本」预览卡（失败内联，不写盘）。
    pub fn assistant_finished(&mut self, reply: &str) {
        self.assistant_busy = false;
        let reply = reply.trim();
        if let Some(message) = self.assistant_messages.last_mut() {
            if message.role == AssistantRole::Assistant {
                if !reply.is_empty() {
                    message.text = reply.to_owned();
                }
                message.streaming = false;
            }
        }
        self.assistant_scroll.scroll_to_bottom();
        self.assistant_patch = None;
        self.assistant_error = None;
        if reply.is_empty() {
            return;
        }
        // 只有回复里出现可解析的 fenced JSON 才生成提案；纯解释回复不报错。
        if experience_assistant::extract_recipe_json(reply).is_none() {
            return;
        }
        let selected_recipe = self.selected().map(|experience| experience.recipe.clone());
        let Some(recipe) = selected_recipe else {
            self.assistant_error = Some("请先在左侧选择一条经验，再应用 AI 建议".to_owned());
            return;
        };
        match experience_assistant::proposal(&recipe, reply) {
            Ok(patch) => self.assistant_patch = Some(patch),
            Err(error) => self.assistant_error = Some(error),
        }
    }

    /// AI 调用失败：定稿占位消息 + 系统提示 + busy=false。
    pub fn assistant_failed(&mut self, error: &str) {
        self.assistant_busy = false;
        let error = error.trim();
        if let Some(message) = self.assistant_messages.last_mut() {
            if message.role == AssistantRole::Assistant {
                message.streaming = false;
                if message.text.trim().is_empty() {
                    message.text = format!("（助手调用失败：{error}）");
                }
            }
        }
        push_assistant_message(
            &mut self.assistant_messages,
            AssistantMessage::system(format!("助手调用失败：{error}")),
        );
        self.assistant_scroll.scroll_to_bottom();
    }

    /// 清空助手会话（切换经验 / 关闭页面时调用，避免误导）。
    pub fn reset_assistant(&mut self) {
        self.assistant_busy = false;
        self.assistant_messages.clear();
        self.assistant_patch = None;
        self.assistant_error = None;
    }

    /// 应用预览卡：取出待保存 recipe（取出后卡片消失，由 `RdmApp` 调 store.update）。
    pub fn take_patch_recipe(&mut self) -> Option<Recipe> {
        let patch = self.assistant_patch.take()?;
        self.assistant_error = None;
        Some(patch.recipe)
    }

    /// 丢弃预览卡。
    pub fn discard_patch(&mut self) {
        self.assistant_patch = None;
        self.assistant_error = None;
    }

    /// 应用新版本失败时的行内错误（由 `RdmApp` 写入）。
    pub fn set_patch_error(&mut self, error: Option<String>) {
        self.assistant_error = error;
    }

    /// 折叠/展开助手（右侧竖条按钮 / 面板头按钮；`RdmApp` 用回调驱动）。
    pub fn toggle_assistant(&mut self) {
        if self.assistant_last_visible.get() {
            self.assistant_open = false;
            self.assistant_narrow_open = false;
        } else {
            self.assistant_open = true;
            self.assistant_narrow_open = true;
        }
    }

    /// 宽窗用户偏好是否展开助手。
    #[allow(dead_code)] // 渲染无副作用读取 + 测试
    pub fn assistant_prefers_open(&self) -> bool {
        self.assistant_open
    }

    /// 计算助手本帧是否可见并记录（`toggle_assistant` 取反用）。窄窗默认折叠。
    fn assistant_visible(&self, viewport_width: f32) -> bool {
        let narrow = viewport_width < ASSISTANT_BREAKPOINT;
        let visible = if narrow {
            self.assistant_narrow_open
        } else {
            self.assistant_open
        };
        self.assistant_last_visible.set(visible);
        visible
    }
}

/// 编辑表单字段（订阅时按字段把文本写回镜像）。
#[derive(Debug, Clone, Copy)]
enum EditField {
    Name,
    Description,
    Hosts,
    UrlPattern,
    Priority,
    Json,
}

/// 订阅一个写入 `Rc<RefCell<String>>` 镜像的单行/多行输入框（URL / 助手输入）。
fn subscribe_text_mirror(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<RdmApp>,
    mirror: &Rc<RefCell<String>>,
) -> Subscription {
    let mirror = mirror.clone();
    cx.subscribe_in(
        input,
        window,
        move |_this: &mut RdmApp,
              input: &Entity<InputState>,
              event: &InputEvent,
              _window: &mut Window,
              cx: &mut Context<RdmApp>| {
            if matches!(event, InputEvent::Change) {
                *mirror.borrow_mut() = input.read(cx).value().to_string();
                cx.notify();
            }
        },
    )
}

/// 订阅一个编辑输入框：内容变化时同步到镜像并刷新页面。
fn subscribe_edit_field(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<RdmApp>,
    mirror: &Rc<RefCell<EditFormValues>>,
    field: EditField,
) -> Subscription {
    let mirror = mirror.clone();
    cx.subscribe_in(
        input,
        window,
        move |_this: &mut RdmApp,
              input: &Entity<InputState>,
              event: &InputEvent,
              _window: &mut Window,
              cx: &mut Context<RdmApp>| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let value = input.read(cx).value().to_string();
            if let Ok(mut values) = mirror.try_borrow_mut() {
                match field {
                    EditField::Name => values.name = value,
                    EditField::Description => values.description = value,
                    EditField::Hosts => values.hosts = value,
                    EditField::UrlPattern => values.url_pattern = value,
                    EditField::Priority => values.priority = value,
                    EditField::Json => values.json = value,
                }
            }
            cx.notify();
        },
    )
}

// ---------------------------------------------------------------------------
// 回调（由 `RdmApp` 用 `cx.listener` 构造）
// ---------------------------------------------------------------------------

/// 无参页面动作。
pub type ExperienceSimpleCallback = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;
/// 携带经验 id 的页面动作。
pub type ExperienceIdCallback = Box<dyn Fn(&str, &ClickEvent, &mut Window, &mut App) + 'static>;
/// 携带经验 id + 版本号的页面动作。
pub type ExperienceVersionCallback =
    Box<dyn Fn(&str, u32, &ClickEvent, &mut Window, &mut App) + 'static>;
/// 携带一段文本（经验助手用户消息）的页面动作。
pub type ExperienceTextCallback = Box<dyn Fn(&str, &ClickEvent, &mut Window, &mut App) + 'static>;
/// 「使用此经验解析」：`(experience_id, url)`。
pub type ExperienceApplyCallback =
    Box<dyn Fn(&str, &str, &ClickEvent, &mut Window, &mut App) + 'static>;
/// 同类合并：`(target_id, source_ids)`。
pub type ExperienceMergeCallback =
    Box<dyn Fn(&str, &[String], &ClickEvent, &mut Window, &mut App) + 'static>;

/// 页面动作集合（字段名与任务书 / `RdmApp` 接线一致）。
pub struct ExperienceCallbacks {
    // ---- v1（design 17）--------------------------------------------------
    pub on_select: ExperienceIdCallback,
    pub on_select_version: ExperienceVersionCallback,
    pub on_set_active: ExperienceVersionCallback,
    pub on_open_edit: ExperienceIdCallback,
    pub on_save_edit: ExperienceSimpleCallback,
    pub on_cancel_edit: ExperienceSimpleCallback,
    pub on_confirm_delete: ExperienceIdCallback,
    pub on_confirm_delete_yes: ExperienceSimpleCallback,
    pub on_cancel_delete: ExperienceSimpleCallback,
    pub on_refresh: ExperienceSimpleCallback,
    pub on_open_resolve_page: ExperienceSimpleCallback,
    /// v1 的「用此经验解析」入口（design 18 起由 [`Self::on_apply_experience`] 取代；
    /// 字段保留以兼容 `RdmApp` 的增量接线，页面不再渲染）。
    pub on_use_experience: ExperienceIdCallback,
    // ---- design 18 §3.1：使用经验 / 同类合并 ----------------------------
    /// 详情顶部「使用此经验解析」行里点「解析」：`(experience_id, url)`。
    pub on_apply_experience: ExperienceApplyCallback,
    /// 同类合并 banner「合并到最早的经验」：`(target_id, source_ids)`。
    pub on_merge_group: ExperienceMergeCallback,
    /// 同类合并 banner「稍后」。
    pub on_dismiss_merge: ExperienceSimpleCallback,
    // ---- design 18 §3.2：经验助手 ---------------------------------------
    /// 发送用户消息（chips 传 `QuickAction::prompt()`，按钮传输入框文本）。
    pub on_send_assistant: ExperienceTextCallback,
    /// 停止生成（中断在途 AI 请求）。
    pub on_stop_assistant: ExperienceSimpleCallback,
    /// 应用 AI 建议（`RdmApp` 取 `take_patch_recipe` 后 `ExperienceStore::update`）。
    pub on_apply_patch: ExperienceSimpleCallback,
    /// 丢弃 AI 建议预览卡。
    pub on_discard_patch: ExperienceSimpleCallback,
    /// AI 未配置时的「去设置」入口。
    pub on_open_settings_ai: ExperienceSimpleCallback,
    /// 折叠/展开右侧经验助手（窄窗折叠为按钮）。
    pub on_toggle_assistant: ExperienceSimpleCallback,
}

type SimpleCb = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type IdCb = Rc<dyn Fn(&str, &ClickEvent, &mut Window, &mut App)>;
type VersionCb = Rc<dyn Fn(&str, u32, &ClickEvent, &mut Window, &mut App)>;
type TextCb = Rc<dyn Fn(&str, &ClickEvent, &mut Window, &mut App)>;
type ApplyCb = Rc<dyn Fn(&str, &str, &ClickEvent, &mut Window, &mut App)>;
type MergeCb = Rc<dyn Fn(&str, &[String], &ClickEvent, &mut Window, &mut App)>;

/// 页面内共享的回调集合（同一动作会在多处复用）。
struct Cbs {
    select: IdCb,
    select_version: VersionCb,
    set_active: VersionCb,
    open_edit: IdCb,
    save_edit: SimpleCb,
    cancel_edit: SimpleCb,
    confirm_delete: IdCb,
    confirm_delete_yes: SimpleCb,
    cancel_delete: SimpleCb,
    refresh: SimpleCb,
    open_resolve_page: SimpleCb,
    /// v1 入口复用为「展开/收起 URL 输入」（由 `RdmApp` 调 begin/cancel）。
    use_experience: IdCb,
    apply_experience: ApplyCb,
    merge_group: MergeCb,
    dismiss_merge: SimpleCb,
    send_assistant: TextCb,
    stop_assistant: SimpleCb,
    apply_patch: SimpleCb,
    discard_patch: SimpleCb,
    open_settings_ai: SimpleCb,
    toggle_assistant: SimpleCb,
}

impl Cbs {
    fn new(callbacks: ExperienceCallbacks) -> Self {
        Self {
            select: Rc::from(callbacks.on_select),
            select_version: Rc::from(callbacks.on_select_version),
            set_active: Rc::from(callbacks.on_set_active),
            open_edit: Rc::from(callbacks.on_open_edit),
            save_edit: Rc::from(callbacks.on_save_edit),
            cancel_edit: Rc::from(callbacks.on_cancel_edit),
            confirm_delete: Rc::from(callbacks.on_confirm_delete),
            confirm_delete_yes: Rc::from(callbacks.on_confirm_delete_yes),
            cancel_delete: Rc::from(callbacks.on_cancel_delete),
            refresh: Rc::from(callbacks.on_refresh),
            open_resolve_page: Rc::from(callbacks.on_open_resolve_page),
            use_experience: Rc::from(callbacks.on_use_experience),
            // design 18：用新 URL 应用经验（而不是重放来源地址）。
            apply_experience: Rc::from(callbacks.on_apply_experience),
            merge_group: Rc::from(callbacks.on_merge_group),
            dismiss_merge: Rc::from(callbacks.on_dismiss_merge),
            send_assistant: Rc::from(callbacks.on_send_assistant),
            stop_assistant: Rc::from(callbacks.on_stop_assistant),
            apply_patch: Rc::from(callbacks.on_apply_patch),
            discard_patch: Rc::from(callbacks.on_discard_patch),
            open_settings_ai: Rc::from(callbacks.on_open_settings_ai),
            toggle_assistant: Rc::from(callbacks.on_toggle_assistant),
        }
    }
}

fn simple_click(cb: &SimpleCb) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(event, window, cx)
}

fn id_click(cb: &IdCb, id: String) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(id.as_str(), event, window, cx)
}

fn version_click(
    cb: &VersionCb,
    id: String,
    version: u32,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(id.as_str(), version, event, window, cx)
}

fn text_click(cb: &TextCb, text: String) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(text.as_str(), event, window, cx)
}

fn apply_click(
    cb: &ApplyCb,
    id: String,
    url: String,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(id.as_str(), url.as_str(), event, window, cx)
}

fn merge_click(
    cb: &MergeCb,
    target: String,
    sources: Vec<String>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let cb = cb.clone();
    move |event, window, cx| cb(target.as_str(), sources.as_slice(), event, window, cx)
}

// ---------------------------------------------------------------------------
// 页面入口
// ---------------------------------------------------------------------------

/// 渲染所需的窗口级只读属性（由 `RdmApp` 每帧计算，页面不持有窗口）。
#[derive(Debug, Clone, Copy)]
pub struct ExperienceViewProps {
    /// 窗口内容宽度（px）：< 1180 时经验助手默认折叠为右侧按钮。
    pub viewport_width: f32,
    /// 是否已配置可用的 AI Provider（未配置时助手显示「去设置」空态）。
    pub ai_configured: bool,
}

impl Default for ExperienceViewProps {
    fn default() -> Self {
        Self {
            viewport_width: 1440.0,
            ai_configured: false,
        }
    }
}

/// 渲染整页（三栏：列表 300 | 详情 flex | 经验助手 340 + 页内模态层）。
pub fn render_experience_page(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    props: ExperienceViewProps,
    callbacks: ExperienceCallbacks,
) -> AnyElement {
    let t = *t;
    let cb = Cbs::new(callbacks);
    let visible = state.visible();
    let assistant_visible = state.assistant_visible(props.viewport_width);

    let mut columns = h_flex()
        .flex_1()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .w_full()
        .child(list_column(&t, state, &visible, &cb))
        .child(detail_area(&t, state, &cb));
    if assistant_visible {
        columns = columns.child(assistant_column(&t, state, props, &cb));
    } else {
        columns = columns.child(assistant_rail(&t, &cb));
    }

    let page = v_flex()
        .relative()
        .size_full()
        .bg(t.bg_app)
        .text_color(t.text_primary)
        .font(theme::ui_font())
        .child(page_header(&t, state, &cb))
        .child(columns);

    let modal: Option<AnyElement> = if state.edit.is_some() {
        Some(edit_modal(&t, state, &cb))
    } else if state.delete.is_some() {
        Some(delete_modal(&t, state, &cb))
    } else {
        None
    };

    match modal {
        Some(modal) => page.child(modal).into_any_element(),
        None => page.into_any_element(),
    }
}

// ---------------------------------------------------------------------------
// 页头
// ---------------------------------------------------------------------------

fn page_header(t: &ThemeTokens, state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let now = now_ms();
    let subtitle = format!(
        "{} 条经验 · {}刷新",
        state.len(),
        format_relative(now, Some(state.loaded_ms()))
    );
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(px(10.0))
        .px(px(PAGE_PAD))
        .py(px(12.0))
        .bg(t.bg_surface)
        .border_b_1()
        .border_color(t.border_subtle)
        .child(RdmIcon::BookOpen.sized(20.0, t.accent))
        .child(
            v_flex()
                .flex_none()
                .gap(px(1.0))
                .child(
                    div()
                        .text_size(px(FS_H1))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_primary)
                        .child("解析经验"),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(subtitle),
                ),
        )
        .child(div().flex_1())
        .child(
            Input::new(&state.search_input)
                .small()
                .w(px(240.0))
                .cleanable(true)
                .prefix(RdmIcon::Search.sized(14.0, t.text_tertiary)),
        )
        .child(
            Button::new("experience-refresh")
                .small()
                .outline()
                .icon(RdmIcon::Refresh.icon())
                .label("刷新")
                .tooltip("重新读取本机经验库；AI 解析成功后会自动刷新")
                .on_click(simple_click(&cb.refresh)),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 左列表
// ---------------------------------------------------------------------------

fn list_column(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    visible: &[&Experience],
    cb: &Cbs,
) -> AnyElement {
    let mut list = v_flex()
        .id("experience-list")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_y_scroll()
        .p(px(6.0))
        .gap(px(2.0));

    if visible.is_empty() {
        let empty = if state.experiences.is_empty() {
            list_empty(
                t,
                RdmIcon::BookOpen,
                "还没有解析经验",
                "AI 成功解析一次后会自动沉淀经验；也可以先去资源解析页试一次。",
                Some(
                    Button::new("experience-empty-resolve")
                        .small()
                        .primary()
                        .icon(RdmIcon::Sparkle.icon())
                        .label("去资源解析")
                        .on_click(simple_click(&cb.open_resolve_page))
                        .into_any_element(),
                ),
            )
        } else {
            list_empty(
                t,
                RdmIcon::Search,
                "没有匹配的经验",
                "换个关键词试试；搜索会匹配名称、主机、说明与来源 URL。",
                None,
            )
        };
        list = list.child(empty);
    } else {
        let now = now_ms();
        for experience in visible {
            list = list.child(experience_row(t, state, experience, now, cb));
        }
    }

    v_flex()
        .flex_none()
        .w(px(LIST_W))
        .h_full()
        .bg(t.bg_surface)
        .border_r_1()
        .border_color(t.border_subtle)
        .child(list)
        .into_any_element()
}

fn experience_row(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    experience: &Experience,
    now: u64,
    cb: &Cbs,
) -> AnyElement {
    let selected = state.selected_id.as_deref() == Some(experience.id.as_str());
    let hover_bg = if selected {
        t.accent_soft_hover
    } else {
        t.bg_hover
    };
    let display_name = experience.display_name();
    let scope_label = scope_text(experience);
    let uses = experience.use_count;
    let last_used = experience.last_used_ms;
    let recent = format_relative(now, last_used);
    let tooltip_text = last_used
        .map(|ms| format!("最近使用：{}", format_utc_datetime(ms)))
        .unwrap_or_else(|| "从未使用".to_owned());
    let row_id = experience.id.clone();
    let recent_id = ElementId::Name(format!("experience-recent-{}", experience.id).into());

    v_flex()
        .id(ElementId::Name(
            format!("experience-row-{}", experience.id).into(),
        ))
        .relative()
        .w_full()
        .gap(px(3.0))
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(RADIUS_SM))
        .cursor_pointer()
        .bg(if selected {
            t.accent_soft
        } else {
            t.bg_surface
        })
        .hover(move |style| style.bg(hover_bg))
        .on_click(id_click(&cb.select, row_id))
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .w(px(2.0))
                    .rounded(px(1.0))
                    .bg(t.accent),
            )
        })
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(FS_BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_primary)
                        .child(display_name),
                )
                .child(soft_tag(
                    format!("v{}", experience.active_version),
                    t.text_secondary,
                    t.bg_hover,
                )),
        )
        .child(
            div()
                .w_full()
                .truncate()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(scope_label),
        )
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(health_pill(t, &experience.health))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_secondary)
                        .child(list_richness_label(experience)),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(if uses > 0 {
                            format!("使用 {uses} 次")
                        } else {
                            "未使用".to_owned()
                        }),
                )
                .when(!experience.related.is_empty(), |this| {
                    this.child(soft_tag("同类", t.warning, t.warning_soft))
                })
                .child(
                    div()
                        .id(recent_id)
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .tooltip(move |window, cx| {
                            Tooltip::new(tooltip_text.clone()).build(window, cx)
                        })
                        .child(recent),
                ),
        )
        .into_any_element()
}

fn list_empty(
    t: &ThemeTokens,
    icon: RdmIcon,
    title: &str,
    description: &str,
    action: Option<AnyElement>,
) -> AnyElement {
    v_flex()
        .w_full()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .px(px(16.0))
        .py(px(36.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(40.0))
                .rounded(px(RADIUS_LG))
                .bg(t.bg_sunken)
                .child(icon.sized(20.0, t.text_tertiary)),
        )
        .child(
            div()
                .text_size(px(FS_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_secondary)
                .child(title.to_owned()),
        )
        .child(
            div()
                .max_w(px(240.0))
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(description.to_owned()),
        )
        .children(action)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 右侧详情
// ---------------------------------------------------------------------------

/// 详情面板里一节流程（主流程或补充流程）。
struct FlowSection<'a> {
    primary: bool,
    /// 「主流程」或补充流程的展示名（recipe.name 或 补充流程 N）。
    name: String,
    recipe_id: String,
    version: u32,
    resources: u32,
    flow: &'a ExperienceFlow,
}

/// 详情要渲染的流程：历史版本只有一条；active 版本先主流程再补充流程；
/// 旧数据（core 未写 `flows`）回退到 `experience.flow` 作为唯一主流程。
fn flow_sections<'a>(
    experience: &'a Experience,
    viewed: Option<&'a ExperienceVersion>,
) -> Vec<FlowSection<'a>> {
    if let Some(version) = viewed.filter(|version| version.version != experience.active_version) {
        return vec![FlowSection {
            primary: true,
            name: "主流程".to_owned(),
            recipe_id: experience.id.clone(),
            version: version.version,
            resources: version.resources,
            flow: &version.flow,
        }];
    }
    let mut sections: Vec<FlowSection<'a>> = experience
        .flows
        .iter()
        .enumerate()
        .map(|(index, flow)| FlowSection {
            primary: flow.primary,
            name: if flow.primary {
                "主流程".to_owned()
            } else if flow.name.trim().is_empty() {
                format!("补充流程 {}", index + 1)
            } else {
                flow.name.clone()
            },
            recipe_id: flow.recipe_id.clone(),
            version: flow.version,
            resources: flow.resources,
            flow: &flow.flow,
        })
        .collect();
    if sections.is_empty() {
        sections.push(FlowSection {
            primary: true,
            name: "主流程".to_owned(),
            recipe_id: experience.id.clone(),
            version: experience.active_version,
            resources: experience.best_resources,
            flow: &experience.flow,
        });
    }
    sections
}

fn detail_area(t: &ThemeTokens, state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let Some(experience) = state.selected() else {
        let (title, description) = if state.experiences.is_empty() {
            (
                "还没有解析经验",
                "解析成功一次后会自动沉淀经验；到资源解析页粘贴一个网页地址开始。",
            )
        } else {
            (
                "从左侧选择一条经验",
                "选中经验后可查看可复用规则、主流程/补充流程、版本历史，也可以编辑/删除或交给经验助手整理。",
            )
        };
        let actions = vec![Button::new("experience-detail-resolve")
            .small()
            .primary()
            .icon(RdmIcon::Sparkle.icon())
            .label("去资源解析")
            .on_click(simple_click(&cb.open_resolve_page))
            .into_any_element()];
        return v_flex()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .items_center()
            .justify_center()
            .child(empty_state(
                t,
                RdmIcon::BookOpen,
                title,
                description,
                actions,
            ))
            .into_any_element();
    };

    let viewed = state.viewed_version(experience);
    let viewing_history =
        viewed.is_some_and(|version| version.version != experience.active_version);
    let display_version = viewed
        .map(|version| version.version)
        .unwrap_or(experience.active_version);
    let health = viewed
        .map(|version| &version.health)
        .unwrap_or(&experience.health);
    let recipe = viewed
        .map(|version| &version.recipe)
        .unwrap_or(&experience.recipe);
    let sections = flow_sections(experience, viewed);

    let mut body = v_flex()
        .w_full()
        .max_w(px(940.0))
        .gap(px(SECTION_GAP))
        .child(detail_header(
            t,
            state,
            experience,
            display_version,
            health,
            recipe,
            cb,
        ))
        .when(viewing_history, |this| {
            this.child(history_banner(t, experience, display_version, cb))
        });
    if let Some(banner) = merge_banner(t, state, experience, cb) {
        body = body.child(banner);
    }
    body = body
        .child(address_card(t, experience))
        .child(stats_row(t, experience));
    if sections.len() > 1 {
        body = body.child(flow_group_line(t, sections.len() - 1));
    }
    for section in &sections {
        body = body.child(flow_section_card(t, section, viewing_history));
    }
    body = body
        .child(versions_card(t, state, experience, cb))
        .child(tests_card(t, experience));

    let scroll = div()
        .id("experience-detail")
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_center()
                .p(px(PAGE_PAD))
                .child(body),
        );
    scroll.into_any_element()
}

/// 「经验组：主流程 + N 个补充流程」（合并后的分组说明）。
fn flow_group_line(t: &ThemeTokens, extra_flows: usize) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap(px(6.0))
        .rounded(px(RADIUS_XS))
        .bg(t.info_soft)
        .px(px(10.0))
        .py(px(6.0))
        .child(RdmIcon::Archive.sized(13.0, t.info))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(FS_MICRO))
                .text_color(t.info)
                .child(format!("经验组：主流程 + {extra_flows} 个补充流程（一次解析依次执行，按 URL 去重合并资源）")),
        )
        .into_any_element()
}

fn detail_header(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    experience: &Experience,
    display_version: u32,
    health: &RecipeHealth,
    recipe: &Recipe,
    cb: &Cbs,
) -> AnyElement {
    let id = experience.id.clone();
    let viewing_history = display_version != experience.active_version;
    let using = state.use_experience_target() == Some(experience.id.as_str());
    let reusable = scope_badge(t, recipe);
    let flows = experience.flows.len().max(1);

    // 窄详情栏（助手展开时约 640px）分行布局，避免标题/徽章/按钮互相挤压被裁切：
    // 1) 标题 + 版本 + 健康度；2) 可复用/写死/经验组/资源徽章（可换行）；
    // 3) 操作按钮（可换行，右对齐）；4) id + scope；5) 使用经验 URL 行。
    let title_row = h_flex()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(FS_H1))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_primary)
                .child(experience.display_name()),
        )
        .child(soft_tag(
            format!("v{display_version}"),
            t.text_secondary,
            t.bg_hover,
        ))
        .child(health_pill(t, health));

    let badge_row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .gap(px(6.0))
        .child(soft_tag(
            reusable.label,
            reusable.foreground,
            reusable.background,
        ))
        .when(!reusable.leftover.is_empty(), |this| {
            this.child(soft_tag(
                format!("仍写死参数：{}", reusable.leftover.join("、")),
                t.warning,
                t.warning_soft,
            ))
        })
        .when(flows > 1, |this| {
            this.child(soft_tag(
                format!("经验组 · {flows} 个流程"),
                t.info,
                t.info_soft,
            ))
        })
        .when(experience.best_resources > 0, |this| {
            this.child(soft_tag(
                format!("最佳 {} 资源", experience.best_resources),
                t.success,
                t.success_soft,
            ))
        });

    let action_row = h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap(px(6.0))
        .child(
            Button::new("experience-use")
                .small()
                .primary()
                .icon(RdmIcon::Sparkle.icon())
                .label(if using {
                    "收起地址输入"
                } else {
                    "使用此经验解析"
                })
                .disabled(viewing_history)
                .tooltip(if viewing_history {
                    "正在查看历史版本；把该版本设为当前版本后才能使用"
                } else if using {
                    "收起 URL 输入"
                } else {
                    "在一个同类新地址上回放该经验（主流程+补充流程，不调用 AI）"
                })
                .on_click(id_click(&cb.use_experience, id.clone())),
        )
        .child(
            Button::new("experience-edit")
                .small()
                .outline()
                .icon(RdmIcon::Sliders.icon())
                .label("编辑")
                .tooltip("编辑元数据与流程，保存后生成新版本")
                .on_click(id_click(&cb.open_edit, id.clone())),
        )
        .child(
            Button::new("experience-delete")
                .small()
                .danger()
                .icon(RdmIcon::Trash.icon())
                .label("删除")
                .tooltip("删除该经验的所有版本（需二次确认）")
                .on_click(id_click(&cb.confirm_delete, id.clone())),
        );

    let mut row = v_flex()
        .w_full()
        .gap(px(6.0))
        .child(title_row)
        .child(badge_row)
        .child(action_row)
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(mono_text(
                    format!("id: {}", experience.id),
                    FS_MONO_MICRO,
                    t.text_tertiary,
                ))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(scope_text(experience)),
                ),
        );
    if using {
        row = row.child(use_experience_row(t, state, experience, cb));
    }
    row.into_any_element()
}

/// 详情顶部展开的单行 URL 输入 + 解析按钮（design 18 §3.1）。
fn use_experience_row(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    experience: &Experience,
    cb: &Cbs,
) -> AnyElement {
    let url = state.use_url_text();
    let validation = validate_experience_url(&url);
    let error = state
        .use_error()
        .map(str::to_owned)
        .or_else(|| validation.as_ref().err().cloned());
    let can_apply = validation.is_ok();
    let id = experience.id.clone();

    v_flex()
        .w_full()
        .gap(px(6.0))
        .rounded(px(RADIUS_XS))
        .bg(t.bg_app)
        .border_1()
        .border_color(if error.is_some() {
            t.danger
        } else {
            t.border_subtle
        })
        .p(px(10.0))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_secondary)
                        .child("在同类新地址上回放（主流程 + 补充流程，不调用 AI）"),
                )
                .child(
                    Button::new("experience-use-cancel")
                        .small()
                        .ghost()
                        .label("收起")
                        .on_click(id_click(&cb.use_experience, id.clone())),
                )
                .child(
                    Button::new("experience-use-apply")
                        .small()
                        .primary()
                        .icon(RdmIcon::Play.icon())
                        .label("解析")
                        .disabled(!can_apply)
                        .on_click(apply_click(&cb.apply_experience, id, url)),
                ),
        )
        .child(Input::new(&state.use_input).small().w_full())
        .when_some(error, |this, error| {
            this.child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.danger)
                    .child(error),
            )
        })
        .into_any_element()
}

/// 窄详情栏（助手展开时约 640px）也能完整显示的提示条。
///
/// `ui::widgets::info_banner` 是单行 flex：正文和按钮挤在一行，640px 下会被裁切
/// （正文被截断、按钮跑到可视区外）。这里改成两行：
/// 1. 正文占满整行（`flex_1 + min_w(0)`，可换行）；
/// 2. 按钮组独立一行、`flex_wrap + justify_end`，窄宽时自动换行，保证每个按钮完整可见。
fn responsive_banner(
    t: &ThemeTokens,
    icon: RdmIcon,
    severity: Severity,
    text: impl Into<SharedString>,
    actions: Vec<AnyElement>,
) -> AnyElement {
    let (foreground, background) = match severity {
        Severity::Info => (t.info, t.info_soft),
        Severity::Warning => (t.warning, t.warning_soft),
        Severity::Danger => (t.danger_text, t.danger_soft),
        Severity::Success => (t.success, t.success_soft),
    };
    let mut body = v_flex()
        .w_full()
        .gap(px(6.0))
        .px(px(12.0))
        .py(px(8.0))
        .bg(background)
        .text_size(px(FS_CAPTION))
        .text_color(foreground)
        .child(
            h_flex()
                .w_full()
                .items_start()
                .gap(px(8.0))
                .child(icon.sized(14.0, foreground))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_CAPTION))
                        .text_color(foreground)
                        .child(text.into()),
                ),
        );
    if !actions.is_empty() {
        body = body.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .justify_end()
                .gap(px(6.0))
                .children(actions),
        );
    }
    body.into_any_element()
}

/// 同类合并提示（design 18 §3.1）：`related` 非空时展示，可合并到最早的经验。
fn merge_banner(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    experience: &Experience,
    cb: &Cbs,
) -> Option<AnyElement> {
    if experience.related.is_empty() || state.merge_hint_dismissed() {
        return None;
    }
    let plan = state.related_merge_plan()?;
    let target_name = state
        .experiences
        .iter()
        .find(|candidate| candidate.id == plan.target_id)
        .map(Experience::display_name)
        .unwrap_or_else(|| plan.target_id.clone());
    let kinds = merge_kind_hint(experience);
    let text = merge_banner_text(experience.related.len(), kinds.as_str());
    Some(responsive_banner(
        t,
        RdmIcon::Archive,
        Severity::Info,
        text,
        vec![
            Button::new("experience-merge-target")
                .small()
                .primary()
                .icon(RdmIcon::Archive.icon())
                .label("合并到最早的经验")
                .tooltip(format!(
                    "目标：{target_name}（同类经验中最早创建）；合并只改元数据，不重写流程 JSON"
                ))
                .on_click(merge_click(
                    &cb.merge_group,
                    plan.target_id,
                    plan.source_ids,
                ))
                .into_any_element(),
            Button::new("experience-merge-later")
                .small()
                .ghost()
                .label("稍后")
                .on_click(simple_click(&cb.dismiss_merge))
                .into_any_element(),
        ],
    ))
}

fn history_banner(t: &ThemeTokens, experience: &Experience, version: u32, cb: &Cbs) -> AnyElement {
    let id = experience.id.clone();
    responsive_banner(
        t,
        RdmIcon::Clock,
        Severity::Info,
        format!(
            "正在查看历史版本 v{version}（当前为 v{}）",
            experience.active_version
        ),
        vec![Button::new("experience-history-set-active")
            .small()
            .primary()
            .label("设为当前版本")
            .on_click(version_click(&cb.set_active, id, version))
            .into_any_element()],
    )
}

// ---------------------------------------------------------------------------
// 详情卡片
// ---------------------------------------------------------------------------

fn card(t: &ThemeTokens) -> Div {
    v_flex()
        .w_full()
        .gap(px(8.0))
        .p(px(12.0))
        .rounded(px(RADIUS_SM))
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_subtle)
}

fn card_header(
    t: &ThemeTokens,
    icon: RdmIcon,
    title: &str,
    subtitle: Option<String>,
    trailing: Option<AnyElement>,
) -> AnyElement {
    let mut row = h_flex()
        .w_full()
        .items_center()
        .gap(px(6.0))
        .child(icon.sized(14.0, t.text_tertiary))
        .child(
            div()
                .flex_none()
                .text_size(px(FS_CAPTION))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_primary)
                .child(title.to_owned()),
        );
    row = match subtitle {
        Some(subtitle) => row.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(subtitle),
        ),
        None => row.child(div().flex_1()),
    };
    row.children(trailing).into_any_element()
}

fn address_card(t: &ThemeTokens, experience: &Experience) -> AnyElement {
    let mut hosts = h_flex().w_full().flex_wrap().items_center().gap(px(4.0));
    if experience.hosts.is_empty() {
        hosts = hosts.child(
            div()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_secondary)
                .child("所有主机（未限制）"),
        );
    } else {
        for host in &experience.hosts {
            hosts = hosts.child(soft_tag(host.clone(), t.text_secondary, t.bg_hover));
        }
    }

    let mut body = card(t)
        .child(card_header(t, RdmIcon::Globe, "适用地址", None, None))
        .child(hosts)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(64.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("URL 规则"),
                )
                .child(match experience.url_pattern.as_ref() {
                    Some(pattern) => mono_truncated(
                        "experience-url-pattern",
                        pattern.clone(),
                        FS_MONO_MICRO,
                        t.text_primary,
                    ),
                    None => div()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child("任意路径")
                        .into_any_element(),
                }),
        );
    if let Some(source_url) = experience.source_url.as_ref() {
        body = body.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(RdmIcon::Link.sized(13.0, t.text_tertiary))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("来源地址"),
                )
                .child(mono_truncated(
                    "experience-source-url",
                    source_url.clone(),
                    FS_MONO_MICRO,
                    t.text_secondary,
                )),
        );
    }
    if let Some(scope) = diagnose_scope(&experience.recipe).scope {
        body = body.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(64.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("可复用范围"),
                )
                .child(mono_text(scope, FS_MONO_MICRO, t.text_secondary)),
        );
    }
    if !experience.resource_kinds.is_empty() {
        body = body.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(64.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("资源种类"),
                )
                .children(
                    experience
                        .resource_kinds
                        .iter()
                        .map(|kind| soft_tag(kind.clone(), t.text_secondary, t.bg_hover)),
                ),
        );
    }
    body.into_any_element()
}

fn stats_row(t: &ThemeTokens, experience: &Experience) -> AnyElement {
    let now = now_ms();
    let success_rate = if experience.use_count > 0 {
        format!(
            "共 {} 次 · 成功 {} / 失败 {}",
            experience.use_count, experience.success_count, experience.failure_count
        )
    } else {
        "尚无调用记录".to_owned()
    };
    h_flex()
        .w_full()
        .flex_wrap()
        .gap(px(8.0))
        .child(stat_block(
            t,
            "创建于",
            format_utc_date(experience.created_ms),
            false,
        ))
        .child(stat_block(
            t,
            "最近更新",
            format_relative(now, Some(experience.updated_ms)),
            false,
        ))
        .child(stat_block(t, "使用情况", success_rate, false))
        .child(stat_block(
            t,
            "流程",
            if experience.flows.len() > 1 {
                format!("主流程 + {} 个补充", experience.flows.len() - 1)
            } else {
                "1 个主流程".to_owned()
            },
            false,
        ))
        .child(stat_block(
            t,
            "最近使用",
            format_relative(now, experience.last_used_ms),
            false,
        ))
        .child(stat_block(
            t,
            "最佳资源",
            if experience.best_resources > 0 {
                format!("{} 个", experience.best_resources)
            } else {
                "未知".to_owned()
            },
            true,
        ))
        .into_any_element()
}

fn stat_block(t: &ThemeTokens, label: &str, value: String, mono: bool) -> AnyElement {
    v_flex()
        .flex_1()
        .min_w(px(136.0))
        .gap(px(2.0))
        .p(px(8.0))
        .rounded(px(RADIUS_XS))
        .bg(t.bg_app)
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(label.to_owned()),
        )
        .child(if mono {
            mono_text(value, FS_MONO_MICRO, t.text_primary).into_any_element()
        } else {
            div()
                .text_size(px(FS_CAPTION))
                .text_color(t.text_primary)
                .child(value)
                .into_any_element()
        })
        .into_any_element()
}

fn flow_section_card(
    t: &ThemeTokens,
    section: &FlowSection<'_>,
    viewing_history: bool,
) -> AnyElement {
    let flow = section.flow;
    let limits = flow.limits;
    let title = if section.primary {
        "主流程".to_owned()
    } else {
        format!("补充流程 · {}", section.name)
    };
    let resources = if section.resources > 0 {
        format!(" · 已知 {} 资源", section.resources)
    } else {
        String::new()
    };
    let subtitle = format!(
        "{}@v{} · 请求 ≤{} · 读取 ≤{} · 时限 {} · 跳转 ≤{}{}",
        section.recipe_id,
        section.version,
        limits.requests,
        format_bytes(limits.bytes_read),
        format_wall_ms(limits.wall_ms),
        limits.redirects,
        resources
    );
    let mut trailing: Vec<AnyElement> = Vec::new();
    if !section.primary {
        trailing.push(soft_tag("补充流程", t.info, t.info_soft).into_any_element());
    }
    if viewing_history {
        trailing.push(soft_tag("历史版本", t.warning, t.warning_soft).into_any_element());
    }
    let trailing = if trailing.is_empty() {
        None
    } else {
        Some(
            h_flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .children(trailing)
                .into_any_element(),
        )
    };
    let element_prefix = format!("{}-{}", section.recipe_id, section.version);

    let mut body = card(t)
        .child(card_header(
            t,
            if section.primary {
                RdmIcon::ListFilter
            } else {
                RdmIcon::Archive
            },
            &title,
            Some(subtitle),
            trailing,
        ))
        .child(divider(t));

    if flow.steps.is_empty() {
        body = body.child(
            div()
                .w_full()
                .py(px(12.0))
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("这个版本没有记录步骤"),
        );
    } else {
        // 「直链型」经验（确定性草稿的 Literal 链）没有页面请求步骤：明确解释，
        // 避免用户看到一个孤零零的断言以为流程丢失。
        let fetches_page = flow.steps.iter().any(|step| {
            matches!(
                step.op.as_str(),
                "http_request" | "browser_read" | "bounded_each" | "url_join" | "template"
            )
        });
        if !fetches_page {
            body = body.child(
                h_flex()
                    .w_full()
                    .gap(px(5.0))
                    .items_center()
                    .rounded(px(RADIUS_XS))
                    .bg(t.bg_app)
                    .px(px(8.0))
                    .py(px(6.0))
                    .child(RdmIcon::Info.sized(12.0, t.info))
                    .child(
                        div()
                            .text_size(px(FS_MICRO))
                            .text_color(t.text_secondary)
                            .child("直链型经验：直接输出已校验的资源地址，无需再请求页面；下面的断言用于固定地址格式。"),
                    ),
            );
        }
        for step in &flow.steps {
            body = body.child(flow_step_row(t, step, element_prefix.as_str()));
        }
    }

    if !flow.captures.is_empty() {
        body = body.child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("地址捕获："),
                )
                .children(
                    flow.captures
                        .iter()
                        .map(|name| soft_tag(name.clone(), t.text_secondary, t.bg_hover)),
                ),
        );
    }
    body = body.child(emit_block(t, flow, element_prefix.as_str()));
    body.into_any_element()
}

fn flow_step_row(t: &ThemeTokens, step: &FlowStep, element_prefix: &str) -> AnyElement {
    let (title, icon) = op_title(&step.op);
    let number = step.index + 1;
    let mut details = v_flex().flex_1().min_w(px(0.0)).gap(px(2.0)).child(
        h_flex()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(FS_CAPTION))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(t.text_primary)
                    .child(format!("{title} · {}", step.id)),
            )
            .child(mono_truncated(
                ElementId::Name(
                    format!("experience-flow-{element_prefix}-summary-{}", step.index).into(),
                ),
                step.summary.clone(),
                FS_MONO_MICRO,
                t.text_secondary,
            )),
    );
    for assignment in &step.assignments {
        details = details.child(
            div()
                .w_full()
                .truncate()
                .font(theme::mono_font())
                .text_size(px(FS_MONO_MICRO))
                .text_color(t.text_tertiary)
                .child(assignment.clone()),
        );
    }
    if !step.expects.is_empty() {
        details = details.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(format!("断言：{}", step.expects.join(" · "))),
        );
    }
    if let Some(condition) = step.condition.as_ref() {
        details = details.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("条件：{condition}")),
        );
    }
    if let Some(on_fail) = step.on_fail.as_ref() {
        details = details.child(
            div()
                .w_full()
                .text_size(px(FS_MICRO))
                .text_color(t.warning)
                .child(format!("失败标记：{on_fail}")),
        );
    }

    h_flex()
        .w_full()
        .items_start()
        .gap(px(8.0))
        .py(px(4.0))
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(20.0))
                .rounded(px(10.0))
                .bg(t.accent_soft)
                .child(
                    div()
                        .font(theme::mono_font())
                        .text_size(px(FS_MONO_MICRO))
                        .text_color(t.accent)
                        .child(number.to_string()),
                ),
        )
        .child(icon.sized(14.0, t.text_tertiary))
        .child(details)
        .into_any_element()
}

fn emit_block(t: &ThemeTokens, flow: &ExperienceFlow, element_prefix: &str) -> AnyElement {
    let emit = &flow.emit;
    let mut body = v_flex()
        .w_full()
        .gap(px(6.0))
        .mt(px(2.0))
        .pt(px(8.0))
        .border_t_1()
        .border_color(t.border_subtle)
        .child(card_header(t, RdmIcon::FileDown, "输出资源", None, None))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(52.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("URL"),
                )
                .child(mono_truncated(
                    ElementId::Name(format!("experience-emit-{element_prefix}-url").into()),
                    emit.url.clone(),
                    FS_MONO_MICRO,
                    t.text_primary,
                )),
        );
    if let Some(name) = emit.name.as_ref() {
        body = body.child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .w(px(52.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("名称"),
                )
                .child(mono_truncated(
                    ElementId::Name(format!("experience-emit-{element_prefix}-name").into()),
                    name.clone(),
                    FS_MONO_MICRO,
                    t.text_secondary,
                )),
        );
    }
    body = body.child(
        h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(px(4.0))
            .when_some(emit.kind.clone(), |this, kind| {
                this.child(soft_tag(kind, t.text_secondary, t.bg_hover))
            })
            .when(emit.one_time, |this| {
                this.child(soft_tag("一次性链接", t.warning, t.warning_soft))
            })
            .when(emit.single_stream, |this| {
                this.child(soft_tag("单流", t.info, t.info_soft))
            }),
    );
    body.into_any_element()
}

fn versions_card(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    experience: &Experience,
    cb: &Cbs,
) -> AnyElement {
    let now = now_ms();
    let mut body = card(t).child(card_header(
        t,
        RdmIcon::Archive,
        "版本历史",
        Some(format!("{} 个版本", experience.versions.len())),
        None,
    ));
    if experience.versions.is_empty() {
        body = body.child(
            div()
                .w_full()
                .py(px(8.0))
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("还没有版本记录"),
        );
    }
    for version in &experience.versions {
        let previewed = state.selected_version == Some(version.version)
            || (state.selected_version.is_none() && version.active);
        let row_id = ElementId::Name(
            format!("experience-version-{}-{}", experience.id, version.version).into(),
        );
        let created = version
            .created_ms
            .map(|ms| format_relative(now, Some(ms)))
            .unwrap_or_else(|| "时间未知".to_owned());
        let tooltip_text = version
            .created_ms
            .map(format_utc_datetime)
            .unwrap_or_else(|| "发布时间未知".to_owned());
        let mut row = h_flex()
            .id(row_id)
            .w_full()
            .items_center()
            .gap(px(8.0))
            .h(px(32.0))
            .px(px(8.0))
            .rounded(px(RADIUS_XS))
            .bg(if previewed { t.accent_soft } else { t.bg_app })
            .cursor_pointer()
            .on_click(version_click(
                &cb.select_version,
                experience.id.clone(),
                version.version,
            ))
            .child(soft_tag(
                format!("v{}", version.version),
                if version.active {
                    t.accent
                } else {
                    t.text_secondary
                },
                t.bg_hover,
            ))
            .when(version.active, |this| {
                this.child(soft_tag("当前", t.success, t.success_soft))
            })
            .child(health_pill(t, &version.health))
            .child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_secondary)
                    .child(if version.resources > 0 {
                        format!("{} 资源", version.resources)
                    } else {
                        "资源数未知".to_owned()
                    }),
            )
            .child(
                div()
                    .id(ElementId::Name(
                        format!(
                            "experience-version-time-{}-{}",
                            experience.id, version.version
                        )
                        .into(),
                    ))
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_tertiary)
                    .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
                    .child(created),
            )
            .child(div().flex_1());
        if !version.active {
            row = row.child(
                Button::new(ElementId::Name(
                    format!(
                        "experience-set-active-{}-{}",
                        experience.id, version.version
                    )
                    .into(),
                ))
                .small()
                .ghost()
                .label("设为当前")
                .on_click(version_click(
                    &cb.set_active,
                    experience.id.clone(),
                    version.version,
                )),
            );
        }
        body = body.child(row);
    }
    body.into_any_element()
}

fn tests_card(t: &ThemeTokens, experience: &Experience) -> AnyElement {
    let mut body = card(t).child(card_header(
        t,
        RdmIcon::CheckCircle,
        "验证样例",
        Some(format!("{} 条", experience.tests.len())),
        None,
    ));
    if experience.tests.is_empty() {
        body = body.child(
            div()
                .w_full()
                .py(px(8.0))
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("这条经验还没有附带验证样例"),
        );
        return body.into_any_element();
    }
    for (index, test) in experience.tests.iter().enumerate() {
        body = body.child(test_row(t, test, index));
    }
    body.into_any_element()
}

fn test_row(t: &ThemeTokens, test: &RecipeTest, index: usize) -> AnyElement {
    let (label, fg, bg) = if test.skip.is_some() {
        ("跳过", t.warning, t.warning_soft)
    } else if test.expect_no_match {
        ("不应匹配", t.text_secondary, t.bg_hover)
    } else {
        ("应匹配", t.success, t.success_soft)
    };
    let mut row = v_flex()
        .w_full()
        .gap(px(2.0))
        .py(px(4.0))
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(soft_tag(label, fg, bg))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_primary)
                        .child(test.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(format!("#{}", index + 1)),
                ),
        )
        .child(mono_truncated(
            ElementId::Name(format!("experience-test-url-{index}").into()),
            test.input_url.clone(),
            FS_MONO_MICRO,
            t.text_secondary,
        ));
    if let Some(reason) = test.skip.as_ref() {
        row = row.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(format!("跳过原因：{reason}")),
        );
    }
    row.into_any_element()
}

// ---------------------------------------------------------------------------
// 右侧经验助手（design 18 §3.2）
// ---------------------------------------------------------------------------

/// 助手折叠后的右侧竖条按钮（窄窗默认折叠；design 18 §3.1）。
fn assistant_rail(t: &ThemeTokens, cb: &Cbs) -> AnyElement {
    v_flex()
        .id("experience-assistant-rail")
        .flex_none()
        .w(px(ASSISTANT_RAIL_W))
        .h_full()
        .items_center()
        .gap(px(10.0))
        .py(px(12.0))
        .bg(t.bg_surface)
        .border_l_1()
        .border_color(t.border_subtle)
        .child(
            Button::new("experience-assistant-toggle")
                .small()
                .ghost()
                .icon(RdmIcon::Bot.icon())
                .tooltip("展开经验助手（窄窗可折叠为按钮）")
                .on_click(simple_click(&cb.toggle_assistant)),
        )
        .child(
            v_flex()
                .items_center()
                .gap(px(1.0))
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("经验"),
                )
                .child(
                    div()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child("助手"),
                ),
        )
        .into_any_element()
}

/// 经验助手面板（design 18 §3.2）：消息流 + 预览卡 + 快捷 chips + 输入框。
fn assistant_column(
    t: &ThemeTokens,
    state: &ExperiencePageState,
    props: ExperienceViewProps,
    cb: &Cbs,
) -> AnyElement {
    let selected_name = state
        .selected()
        .map(Experience::display_name)
        .unwrap_or_else(|| "未选择经验".to_owned());
    let mut panel = v_flex()
        .id("experience-assistant")
        .flex_none()
        .w(px(ASSISTANT_W))
        .h_full()
        .min_h(px(0.0))
        .bg(t.bg_surface)
        .border_l_1()
        .border_color(t.border_subtle)
        .child(assistant_header(t, &selected_name, cb))
        .child(divider(t));

    if !props.ai_configured {
        panel = panel.child(assistant_not_configured(t, cb));
    } else {
        panel = panel.child(assistant_messages_area(t, state));
        if let Some(error) = state.assistant_patch_error() {
            panel = panel.child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .pb(px(6.0))
                    .child(info_banner(
                        t,
                        RdmIcon::Alert,
                        Severity::Danger,
                        error.to_owned(),
                        vec![],
                    )),
            );
        }
        if let Some(patch) = state.patch_preview() {
            panel = panel.child(patch_preview_card(t, patch, cb));
        }
        panel = panel
            .child(assistant_chips(state, cb))
            .child(assistant_composer(t, state, cb));
    }
    panel.into_any_element()
}

fn assistant_header(t: &ThemeTokens, selected_name: &str, cb: &Cbs) -> AnyElement {
    h_flex()
        .flex_none()
        .w_full()
        .items_center()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.0))
                .rounded(px(RADIUS_SM))
                .bg(t.accent_soft)
                .child(RdmIcon::Bot.sized(15.0, t.accent)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(1.0))
                .child(
                    div()
                        .text_size(px(FS_CAPTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_primary)
                        .child("经验助手"),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(selected_name.to_owned()),
                ),
        )
        .child(
            Button::new("experience-assistant-collapse")
                .ghost()
                .compact()
                .icon(RdmIcon::PanelRightClose.icon())
                .tooltip("收起经验助手（窄窗折叠为右侧按钮）")
                .on_click(simple_click(&cb.toggle_assistant)),
        )
        .into_any_element()
}

/// AI 未配置空态：说明能力 + 去设置入口（不回退假回复）。
fn assistant_not_configured(t: &ThemeTokens, cb: &Cbs) -> AnyElement {
    v_flex()
        .flex_1()
        .min_h(px(0.0))
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .px(px(18.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(44.0))
                .rounded(px(RADIUS_LG))
                .bg(t.bg_sunken)
                .child(RdmIcon::Bot.sized(22.0, t.text_tertiary)),
        )
        .child(
            div()
                .text_size(px(FS_CAPTION))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_secondary)
                .child("配置 AI 后可让助手整理经验"),
        )
        .child(
            div()
                .text_center()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("助手会基于当前经验生成可复用模板 / 修复写死参数，并在应用前给你预览；不配置时不会伪造回复。"),
        )
        .child(
            Button::new("experience-assistant-settings")
                .small()
                .primary()
                .icon(RdmIcon::Settings.icon())
                .label("去设置配置 AI")
                .on_click(simple_click(&cb.open_settings_ai)),
        )
        .into_any_element()
}

fn assistant_messages_area(t: &ThemeTokens, state: &ExperiencePageState) -> AnyElement {
    let mut list = v_flex()
        .id("experience-assistant-messages")
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .overflow_y_scroll()
        .p(px(10.0))
        .gap(px(8.0))
        .track_scroll(&state.assistant_scroll);
    if state.assistant_messages().is_empty() {
        list = list.child(assistant_intro(t, state));
    } else {
        for (index, message) in state
            .assistant_messages()
            .iter()
            .filter(|message| message.is_visible())
            .enumerate()
        {
            list = list.child(assistant_message_row(t, message, index));
        }
    }
    list.into_any_element()
}

fn assistant_intro(t: &ThemeTokens, state: &ExperiencePageState) -> AnyElement {
    let (title, detail) = match state.selected() {
        Some(experience) => (
            format!("正在调试：{}", experience.display_name()),
            "可以让助手把当前经验整理成可复用模板（抽象 scope/捕获变量）、修复写死参数（如 cid），或只解释流程。AI 建议会先给预览卡，点「应用为新版本」才会写盘。",
        ),
        None => (
            "先选择一条经验".to_owned(),
            "从左侧选中一条经验后，助手会以它的完整 recipe 作为上下文；生成的修改建议需要你确认后才会保存为新版本。",
        ),
    };
    v_flex()
        .w_full()
        .gap(px(6.0))
        .rounded(px(RADIUS_SM))
        .bg(t.bg_app)
        .p(px(10.0))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_secondary)
                .child(title),
        )
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(detail),
        )
        .into_any_element()
}

fn assistant_message_row(t: &ThemeTokens, message: &AssistantMessage, index: usize) -> AnyElement {
    match message.role {
        AssistantRole::User => h_flex()
            .w_full()
            .justify_end()
            .child(
                div()
                    .id(ElementId::Name(
                        format!("experience-assistant-user-{index}").into(),
                    ))
                    .max_w(px(280.0))
                    .rounded(px(RADIUS_SM))
                    .bg(t.accent_soft)
                    .px(px(10.0))
                    .py(px(7.0))
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_primary)
                    .child(message.text.clone()),
            )
            .into_any_element(),
        AssistantRole::Assistant => v_flex()
            .id(ElementId::Name(
                format!("experience-assistant-reply-{index}").into(),
            ))
            .w_full()
            .gap(px(4.0))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(RdmIcon::Bot.sized(13.0, t.accent))
                    .child(div().text_size(px(FS_MICRO)).text_color(t.accent).child(
                        if message.streaming {
                            "经验助手 · 生成中…"
                        } else {
                            "经验助手"
                        },
                    ))
                    .when(message.streaming, |this| {
                        this.child(RdmIcon::Loader.sized(12.0, t.info))
                    }),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(FS_CAPTION))
                    .text_color(t.text_primary)
                    .child(if message.text.trim().is_empty() {
                        "正在思考…".to_owned()
                    } else {
                        message.text.clone()
                    }),
            )
            .into_any_element(),
        AssistantRole::System => v_flex()
            .w_full()
            .gap(px(2.0))
            .child(
                div()
                    .text_size(px(FS_MICRO))
                    .text_color(t.warning)
                    .child("系统提示"),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(FS_MICRO))
                    .text_color(t.text_secondary)
                    .child(message.text.clone()),
            )
            .into_any_element(),
    }
}

/// AI 建议预览卡：新版本号 + 关键变化 + 应用/丢弃（校验失败不写盘）。
fn patch_preview_card(t: &ThemeTokens, patch: &PatchProposal, cb: &Cbs) -> AnyElement {
    let mut changes = v_flex().w_full().gap(px(2.0));
    for change in &patch.changes {
        changes = changes.child(
            h_flex()
                .w_full()
                .items_start()
                .gap(px(4.0))
                .child(RdmIcon::ArrowRight.sized(11.0, t.text_tertiary))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_secondary)
                        .child(change.clone()),
                ),
        );
    }
    v_flex()
        .id("experience-assistant-patch")
        .flex_none()
        .w_full()
        .gap(px(6.0))
        .mx(px(10.0))
        .mb(px(6.0))
        .p(px(10.0))
        .rounded(px(RADIUS_SM))
        .bg(t.success_soft)
        .border_1()
        .border_color(t.border_subtle)
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(RdmIcon::Sparkle.sized(14.0, t.success))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_CAPTION))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_primary)
                        .child(format!("建议新版本 v{}", patch.next_version)),
                ),
        )
        .child(changes)
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child("应用前会做 recipe 结构校验；失败只提示，不写盘。"),
        )
        .child(
            h_flex()
                .w_full()
                .justify_end()
                .gap(px(6.0))
                .child(
                    Button::new("experience-assistant-patch-discard")
                        .small()
                        .ghost()
                        .label("丢弃")
                        .on_click(simple_click(&cb.discard_patch)),
                )
                .child(
                    Button::new("experience-assistant-patch-apply")
                        .small()
                        .primary()
                        .icon(RdmIcon::Check.icon())
                        .label("应用为新版本")
                        .on_click(simple_click(&cb.apply_patch)),
                ),
        )
        .into_any_element()
}

fn assistant_chips(state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let disabled = state.assistant_busy() || state.selected().is_none();
    let mut row = h_flex().w_full().flex_wrap().gap(px(4.0)).px(px(10.0));
    for action in [
        QuickAction::MakeReusable,
        QuickAction::FixHardcoded,
        QuickAction::Explain,
    ] {
        let tooltip = match action {
            QuickAction::MakeReusable => "让 AI 抽象匹配范围与捕获变量，输出可复用模板",
            QuickAction::FixHardcoded => "让 AI 用前置请求动态获取内部参数，去掉写死值",
            QuickAction::Explain => "只解释流程与适用链接，不生成新版本",
        };
        row = row.child(
            Button::new(ElementId::Name(
                format!("experience-assistant-chip-{}", action.label_zh()).into(),
            ))
            .small()
            .outline()
            .label(action.label_zh())
            .disabled(disabled)
            .tooltip(tooltip)
            .on_click(text_click(&cb.send_assistant, action.prompt().to_owned())),
        );
    }
    row.into_any_element()
}

fn assistant_composer(t: &ThemeTokens, state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let busy = state.assistant_busy();
    let draft = state.assistant_draft();
    let can_send = !busy && !draft.trim().is_empty() && state.selected().is_some();
    v_flex()
        .flex_none()
        .w_full()
        .gap(px(6.0))
        .px(px(10.0))
        .py(px(8.0))
        .border_t_1()
        .border_color(t.border_subtle)
        .child(Input::new(&state.assistant_input).w_full())
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(FS_MICRO))
                        .text_color(t.text_tertiary)
                        .child(if busy {
                            "AI 正在生成…可点「停止」中断"
                        } else {
                            "Enter 发送 · Shift+Enter 换行；建议先点上方 chips"
                        }),
                )
                .when(busy, |this| {
                    this.child(
                        Button::new("experience-assistant-stop")
                            .small()
                            .danger()
                            .icon(RdmIcon::Square.icon())
                            .label("停止")
                            .on_click(simple_click(&cb.stop_assistant)),
                    )
                })
                .when(!busy, |this| {
                    this.child(
                        Button::new("experience-assistant-send")
                            .small()
                            .primary()
                            .icon(RdmIcon::Send.icon())
                            .label("发送")
                            .disabled(!can_send)
                            .on_click(text_click(&cb.send_assistant, draft)),
                    )
                }),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 编辑 / 删除模态层
// ---------------------------------------------------------------------------

fn edit_modal(t: &ThemeTokens, state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let Some(edit) = state.edit.as_ref() else {
        return div().into_any_element();
    };
    let id = edit.id.clone();
    let error = edit.error.clone();
    let cancel = cb.cancel_edit.clone();

    let mut form = v_flex()
        .id("experience-edit-form")
        .w_full()
        .gap(px(8.0))
        .max_h(px(EDIT_FORM_MAX_H))
        .overflow_y_scroll()
        .child(form_field(
            t,
            "名称",
            Some("用于列表与详情标题；留空时回退为适用主机/来源地址"),
            Input::new(&state.edit_inputs.name).small().w_full(),
        ))
        .child(form_field(
            t,
            "说明",
            None,
            Input::new(&state.edit_inputs.description).small().w_full(),
        ))
        .child(
            h_flex()
                .w_full()
                .flex_wrap()
                .items_start()
                .gap(px(8.0))
                .child(div().flex_1().min_w(px(240.0)).child(form_field(
                    t,
                    "适用主机",
                    Some("逗号分隔；支持 *.example.com 通配"),
                    Input::new(&state.edit_inputs.hosts).small().w_full(),
                )))
                .child(div().flex_none().w(px(160.0)).child(form_field(
                    t,
                    "优先级",
                    Some("整数，越大越先匹配"),
                    Input::new(&state.edit_inputs.priority).small().w_full(),
                ))),
        )
        .child(form_field(
            t,
            "URL 正则",
            Some("留空 = 该主机任意路径；命名捕获会注入流程变量"),
            Input::new(&state.edit_inputs.url_pattern).small().w_full(),
        ))
        .child(form_field(
            t,
            "解析流程 JSON",
            Some("保存前会做结构校验；保存 = 发布新版本，旧版本保留"),
            Input::new(&state.edit_inputs.json).w_full(),
        ));
    if let Some(error) = error {
        form = form.child(div().w_full().child(info_banner(
            t,
            RdmIcon::Alert,
            Severity::Danger,
            error,
            vec![],
        )));
    }

    div()
        .id("experience-edit-overlay")
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .bg(t.bg_overlay)
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .track_focus(&state.modal_focus)
        .on_key_down(
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                if event.keystroke.key == "escape" {
                    cancel(&ClickEvent::default(), window, cx);
                }
            },
        )
        .child(
            v_flex()
                .w(px(EDIT_MODAL_W))
                .gap(px(10.0))
                .p(px(16.0))
                .rounded(px(RADIUS_LG))
                .bg(t.bg_elevated)
                .border_1()
                .border_color(t.border_default)
                .occlude()
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(px(8.0))
                        .child(RdmIcon::Sliders.sized(18.0, t.accent))
                        .child(
                            div()
                                .text_size(px(FS_H1))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_primary)
                                .child("编辑解析经验"),
                        )
                        .child(mono_text(
                            format!("id: {id}"),
                            FS_MONO_MICRO,
                            t.text_tertiary,
                        ))
                        .child(div().flex_1())
                        .child(
                            Button::new("experience-edit-close")
                                .ghost()
                                .compact()
                                .icon(RdmIcon::Close.icon())
                                .tooltip("关闭（Esc）")
                                .on_click(simple_click(&cb.cancel_edit)),
                        ),
                )
                .child(divider(t))
                .child(form)
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(FS_MICRO))
                                .text_color(t.text_tertiary)
                                .child("保存会发布新版本；旧版本与健康度保留，可在版本历史回滚。"),
                        )
                        .child(
                            Button::new("experience-edit-cancel")
                                .small()
                                .outline()
                                .label("取消")
                                .on_click(simple_click(&cb.cancel_edit)),
                        )
                        .child(
                            Button::new("experience-edit-save")
                                .small()
                                .primary()
                                .icon(RdmIcon::Check.icon())
                                .label("保存")
                                .on_click(simple_click(&cb.save_edit)),
                        ),
                ),
        )
        .into_any_element()
}

fn delete_modal(t: &ThemeTokens, state: &ExperiencePageState, cb: &Cbs) -> AnyElement {
    let Some(target) = state.delete.as_ref() else {
        return div().into_any_element();
    };
    let cancel = cb.cancel_delete.clone();
    let name = target.display_name.clone();
    let id = target.id.clone();

    div()
        .id("experience-delete-overlay")
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .bg(t.bg_overlay)
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .track_focus(&state.modal_focus)
        .on_key_down(move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
            if event.keystroke.key == "escape" {
                cancel(&ClickEvent::default(), window, cx);
            }
        })
        .child(
            v_flex()
                .w(px(DELETE_MODAL_W))
                .gap(px(10.0))
                .p(px(16.0))
                .rounded(px(RADIUS_LG))
                .bg(t.bg_elevated)
                .border_1()
                .border_color(t.border_default)
                .occlude()
                .child(
                    h_flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(RdmIcon::Alert.sized(18.0, t.danger))
                        .child(
                            div()
                                .text_size(px(FS_H1))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_primary)
                                .child("删除解析经验"),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(FS_CAPTION))
                        .text_color(t.text_secondary)
                        .child(format!(
                            "确定删除「{name}」？删除后该地址的下一次解析将重新调用 AI 研究，且不可恢复。"
                        )),
                )
                .child(mono_text(id, FS_MONO_MICRO, t.text_tertiary))
                .child(divider(t))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            Button::new("experience-delete-cancel")
                                .small()
                                .outline()
                                .label("取消")
                                .on_click(simple_click(&cb.cancel_delete)),
                        )
                        .child(
                            Button::new("experience-delete-confirm")
                                .small()
                                .danger()
                                .icon(RdmIcon::Trash.icon())
                                .label("确认删除")
                                .on_click(simple_click(&cb.confirm_delete_yes)),
                        ),
                ),
        )
        .into_any_element()
}

fn form_field(
    t: &ThemeTokens,
    label: &str,
    hint: Option<&str>,
    control: impl IntoElement,
) -> AnyElement {
    let mut column = v_flex()
        .w_full()
        .gap(px(4.0))
        .child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_secondary)
                .child(label.to_owned()),
        )
        .child(control);
    if let Some(hint) = hint {
        column = column.child(
            div()
                .text_size(px(FS_MICRO))
                .text_color(t.text_tertiary)
                .child(hint.to_owned()),
        );
    }
    column.into_any_element()
}

// ---------------------------------------------------------------------------
// 通用小部件
// ---------------------------------------------------------------------------

fn health_style(status: MethodStatus, t: &ThemeTokens) -> (RdmIcon, Hsla, Hsla, &'static str) {
    match status {
        MethodStatus::Ok => (RdmIcon::CheckCircle, t.success, t.success_soft, "正常"),
        MethodStatus::Degraded => (RdmIcon::Alert, t.warning, t.warning_soft, "降级"),
        MethodStatus::Broken => (RdmIcon::ErrorCircle, t.danger, t.danger_soft, "失效"),
        MethodStatus::Untested => (RdmIcon::Circle, t.text_secondary, t.bg_hover, "未测试"),
    }
}

fn health_pill(t: &ThemeTokens, health: &RecipeHealth) -> AnyElement {
    let (icon, foreground, background, label) = health_style(health.status, t);
    status_pill(t, icon, label, foreground, background)
}

/// 单行等宽文本：超出截断 + hover tooltip 展示完整内容。
fn mono_truncated(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    size: f32,
    color: Hsla,
) -> AnyElement {
    let full: SharedString = text.into();
    let tooltip_text = full.clone();
    div()
        .id(id)
        .flex_1()
        .min_w(px(0.0))
        .truncate()
        .font(theme::mono_font())
        .text_size(px(size))
        .text_color(color)
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .child(full)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// 纯函数（可单元测试，不依赖 GPUI）
// ---------------------------------------------------------------------------

/// 列表行的丰富度文案：合并后 `N 个流程 · M 个资源`，否则沿用 v1 的「最佳 N 资源」。
fn list_richness_label(experience: &Experience) -> String {
    let flows = experience.flows.len();
    if flows > 1 {
        if experience.best_resources > 0 {
            format!("{flows} 个流程 · {} 个资源", experience.best_resources)
        } else {
            format!("{flows} 个流程 · 资源数未知")
        }
    } else if experience.best_resources > 0 {
        format!("最佳 {} 资源", experience.best_resources)
    } else {
        "资源数未知".to_owned()
    }
}

/// 可复用性诊断结果（design 18 §2.1/§3.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopePolicy {
    /// 已抽象出变量，可跨同类 URL 复用。
    Reusable,
    /// 有 URL 正则但无变量（仅精确地址）。
    Exact,
    /// 只限主机、任意路径。
    PathRule,
}

/// 可复用性诊断（`generalization` 缺省时用 captures / 命名捕获推断）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeDiagnosis {
    policy: ScopePolicy,
    leftover: Vec<String>,
    scope: Option<String>,
}

fn diagnose_scope(recipe: &Recipe) -> ScopeDiagnosis {
    let leftover = recipe
        .generalization
        .as_ref()
        .map(|generalization| {
            generalization
                .leftover_hardcoded
                .iter()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let scope = recipe
        .generalization
        .as_ref()
        .map(|generalization| generalization.scope.trim().to_owned())
        .filter(|scope| !scope.is_empty());
    let pattern = recipe
        .matcher
        .url_pattern
        .as_deref()
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty());
    let policy = if let Some(generalization) = recipe.generalization.as_ref() {
        if generalization.reusable {
            ScopePolicy::Reusable
        } else if pattern.is_some() {
            ScopePolicy::Exact
        } else {
            ScopePolicy::PathRule
        }
    } else {
        // 旧数据（无 generalization）：captures / 命名捕获是「可复用」的确定性信号。
        let has_named_capture = recipe
            .captures
            .iter()
            .any(|capture| !capture.trim().is_empty())
            || pattern.is_some_and(|pattern| pattern.contains("(?P<"));
        if has_named_capture {
            ScopePolicy::Reusable
        } else if pattern.is_some() {
            ScopePolicy::Exact
        } else {
            ScopePolicy::PathRule
        }
    };
    ScopeDiagnosis {
        policy,
        leftover,
        scope,
    }
}

/// 详情顶部可复用规则徽章（文案 + 配色；design 18 §3.1）。
struct ScopeBadge {
    label: &'static str,
    foreground: Hsla,
    background: Hsla,
    leftover: Vec<String>,
}

fn scope_badge(t: &ThemeTokens, recipe: &Recipe) -> ScopeBadge {
    let diagnosis = diagnose_scope(recipe);
    let (label, foreground, background) = match diagnosis.policy {
        ScopePolicy::Reusable => ("可复用规则", t.success, t.success_soft),
        ScopePolicy::PathRule => ("路径规则", t.info, t.info_soft),
        ScopePolicy::Exact => ("仅精确地址", t.warning, t.warning_soft),
    };
    ScopeBadge {
        label,
        foreground,
        background,
        leftover: diagnosis.leftover,
    }
}

/// 列表行/详情头部的 scope 文案（design 18 §3.1）。
///
/// 不能直接用 [`Experience::scope_label`]：v1 只看 `^…$` 就判「精确地址」，而 v2 的
/// matcher 即使以 `^…$` 收尾也可能含 `(?P<bvid>)` + `generalization.reusable=true`。
/// 这里统一按 [`diagnose_scope`] 判定，保证列表与详情徽章一致：
/// 可复用 → 「可复用规则」；无变量正则 → 「精确地址」；只限主机 → 「任意路径」。
fn scope_text(experience: &Experience) -> String {
    let mut hosts: Vec<&str> = Vec::new();
    for host in &experience.hosts {
        let host = host.trim();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    let hosts = if hosts.is_empty() {
        "任意主机".to_owned()
    } else {
        hosts.join("、")
    };
    let rule = match diagnose_scope(&experience.recipe).policy {
        ScopePolicy::Reusable => "可复用规则",
        ScopePolicy::Exact => "精确地址",
        ScopePolicy::PathRule => "任意路径",
    };
    format!("{hosts} · {rule}")
}

/// 经验列表/详情的样本地址（来源 URL 优先，其次第一条验证样例）。
fn experience_sample_url(experience: &Experience) -> Option<String> {
    experience
        .source_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            experience
                .tests
                .first()
                .map(|test| test.input_url.trim())
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
        })
}

/// 同类合并分组键：`generalization.scope` 优先，否则 hosts + url_pattern。
fn merge_scope_key(experience: &Experience) -> String {
    if let Some(scope) = experience
        .recipe
        .generalization
        .as_ref()
        .map(|generalization| generalization.scope.trim())
        .filter(|scope| !scope.is_empty())
    {
        return format!("scope:{scope}");
    }
    let mut hosts = experience.hosts.clone();
    hosts.sort();
    hosts.dedup();
    format!(
        "hosts:{}|pattern:{}",
        hosts.join(","),
        experience.url_pattern.as_deref().unwrap_or("").trim()
    )
}

/// 同类合并计划：目标 = 最早创建的经验；其余作为补充流程来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub target_id: String,
    pub source_ids: Vec<String>,
}

fn choose_merge_plan(selected: &Experience, all: &[Experience]) -> Option<MergePlan> {
    let mut candidates: Vec<&Experience> = vec![selected];
    for related_id in &selected.related {
        let Some(candidate) = all.iter().find(|candidate| &candidate.id == related_id) else {
            continue;
        };
        if candidate.id == selected.id || candidates.iter().any(|item| item.id == candidate.id) {
            continue;
        }
        candidates.push(candidate);
    }
    if candidates.len() < 2 {
        return None;
    }
    // 最早创建者为 target；时间相同按 id 升序，保证确定性。
    candidates.sort_by(|left, right| {
        left.created_ms
            .cmp(&right.created_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let target = candidates[0];
    Some(MergePlan {
        target_id: target.id.clone(),
        source_ids: candidates[1..]
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect(),
    })
}

/// 合并提示里的资源种类摘要（如「image / video」），未知返回空串。
fn merge_kind_hint(experience: &Experience) -> String {
    let mut kinds: Vec<String> = Vec::new();
    let mut push = |kind: Option<&String>| {
        if let Some(kind) = kind {
            let kind = kind.trim();
            if !kind.is_empty() && !kinds.iter().any(|existing| existing == kind) {
                kinds.push(kind.to_owned());
            }
        }
    };
    for flow in &experience.flows {
        push(flow.flow.emit.kind.as_ref());
    }
    push(experience.flow.emit.kind.as_ref());
    kinds.join(" / ")
}

/// 「使用此经验解析」的 URL 校验（只接受 http/https + 主机）。
fn validate_experience_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("请输入同类链接（http/https）".to_owned());
    }
    let parsed =
        url::Url::parse(trimmed).map_err(|_| "链接格式不合法（需 http:// 或 https:// 开头）")?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("只支持 http/https 链接（当前 {other}）")),
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return Err("链接缺少主机名".to_owned());
    }
    Ok(trimmed.to_owned())
}

/// 同类合并 banner 文案（`related` 数量 + 资源互补摘要；纯函数便于单测文案）。
fn merge_banner_text(related_count: usize, kinds: &str) -> String {
    if related_count == 0 {
        return String::new();
    }
    let kinds = kinds.trim();
    if kinds.is_empty() {
        format!("发现 {related_count} 条同类经验，合并后可一次获得全部资源。")
    } else {
        format!("发现 {related_count} 条同类经验（资源互补：{kinds}），合并后可一次获得全部资源。")
    }
}

/// 追加流式增量，并限制单条消息最大字符数（超长时保留尾部）。
fn append_assistant_text(current: &str, delta: &str, max_chars: usize) -> String {
    let mut text = String::with_capacity(current.len() + delta.len());
    text.push_str(current);
    text.push_str(delta);
    let length = text.chars().count();
    if length > max_chars {
        text = text.chars().skip(length - max_chars).collect();
    }
    text
}

/// 追加一条助手消息并裁剪到 `ASSISTANT_MESSAGES_MAX` 条（保留最近的消息）。
fn push_assistant_message(messages: &mut Vec<AssistantMessage>, message: AssistantMessage) {
    messages.push(message);
    if messages.len() > ASSISTANT_MESSAGES_MAX {
        let excess = messages.len() - ASSISTANT_MESSAGES_MAX;
        messages.drain(0..excess);
    }
}

/// 相对时间：刚刚 / N 分钟前 / N 小时前 / N 天前（`None` = 从未）。
fn format_relative(now_ms: u64, then_ms: Option<u64>) -> String {
    let Some(then_ms) = then_ms else {
        return "从未使用".to_owned();
    };
    if then_ms == 0 {
        return "--".to_owned();
    }
    let elapsed = now_ms.saturating_sub(then_ms);
    if elapsed < 60_000 {
        return "刚刚".to_owned();
    }
    let minutes = elapsed / 60_000;
    if minutes < 60 {
        return format!("{minutes} 分钟前");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} 小时前");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days} 天前");
    }
    format!("{} 年前", days / 365)
}

/// 本地无关的绝对时间（UTC）：批量日志/经验时间来自本机，展示时明确标注 UTC。
fn format_utc_datetime(ms: u64) -> String {
    let (year, month, day, hour, minute, _second) = utc_parts(ms);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// 「YYYY-MM-DD」（UTC）。
fn format_utc_date(ms: u64) -> String {
    let (year, month, day, _hour, _minute, _second) = utc_parts(ms);
    format!("{year:04}-{month:02}-{day:02}")
}

fn utc_parts(ms: u64) -> (i64, u32, u32, u32, u32, u32) {
    let total_seconds = ms / 1000;
    let days = (total_seconds / 86_400) as i64;
    let remainder = total_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = (remainder / 3_600) as u32;
    let minute = ((remainder % 3_600) / 60) as u32;
    let second = (remainder % 60) as u32;
    (year, month, day, hour, minute, second)
}

/// Howard Hinnant 的 civil-from-days（1970-01-01 起的天数 → 公历年月日）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

fn format_wall_ms(ms: u64) -> String {
    if ms >= 1_000 {
        if ms.is_multiple_of(1_000) {
            format!("{}s", ms / 1_000)
        } else {
            format!("{:.1}s", ms as f64 / 1_000.0)
        }
    } else {
        format!("{ms}ms")
    }
}

/// 搜索词是否命中任一字段（忽略大小写；空词命中全部）。
fn search_matches_any(query: &str, fields: &[&str]) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    fields
        .iter()
        .any(|field| field.to_lowercase().contains(&query))
}

fn experience_matches(experience: &Experience, query: &str) -> bool {
    let display_name = experience.display_name();
    let scope_label = scope_text(experience);
    let mut fields: Vec<&str> = vec![
        display_name.as_str(),
        scope_label.as_str(),
        experience.id.as_str(),
        experience.description.as_str(),
    ];
    if let Some(source_url) = experience.source_url.as_ref() {
        fields.push(source_url.as_str());
    }
    for host in &experience.hosts {
        fields.push(host.as_str());
    }
    search_matches_any(query, &fields)
}

/// 操作符 → 中文标题 + 图标（设计 §3.1；未知操作保底为「自定义步骤」）。
fn op_title(op: &str) -> (&'static str, RdmIcon) {
    match op {
        "http_request" => ("请求网络", RdmIcon::Globe),
        "json_query" => ("提取 JSON", RdmIcon::ListFilter),
        "regex_capture" => ("正则提取", RdmIcon::Search),
        "url_join" => ("拼接地址", RdmIcon::Link),
        "template" => ("模板", RdmIcon::TextFile),
        "assert" => ("校验", RdmIcon::Shield),
        "branch" => ("条件分支", RdmIcon::ArrowRight),
        "bounded_each" => ("遍历列表", RdmIcon::Inbox),
        _ => ("自定义步骤", RdmIcon::Circle),
    }
}

fn parse_hosts(text: &str) -> Vec<String> {
    text.split([',', '，', ';', '；', '\n', '\r'])
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_priority(text: &str) -> Result<i32, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    trimmed
        .parse::<i32>()
        .map_err(|_| "优先级必须是整数（例如 0 或 10）".to_owned())
}

/// 把编辑表单合成为 recipe（强制 id/版本；失败返回中文错误）。
fn recipe_from_edit_json(
    json_text: &str,
    form: &EditFormValues,
    forced_id: &str,
    forced_version: u32,
) -> Result<Recipe, String> {
    let priority = parse_priority(&form.priority)?;
    let mut value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|error| format!("解析流程 JSON 失败：{error}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "解析流程 JSON 必须是一个对象".to_owned())?;
    object.insert(
        "id".to_owned(),
        serde_json::Value::String(forced_id.to_owned()),
    );
    object.insert(
        "version".to_owned(),
        serde_json::Value::from(forced_version),
    );
    object.insert(
        "name".to_owned(),
        serde_json::Value::String(form.name.trim().to_owned()),
    );
    object.insert(
        "description".to_owned(),
        serde_json::Value::String(form.description.trim().to_owned()),
    );
    let matcher = object
        .entry("match")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let matcher = matcher
        .as_object_mut()
        .ok_or_else(|| "流程 JSON 的 match 必须是对象".to_owned())?;
    matcher.insert(
        "hosts".to_owned(),
        serde_json::Value::Array(
            parse_hosts(&form.hosts)
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    let pattern = form.url_pattern.trim();
    matcher.insert(
        "url_pattern".to_owned(),
        if pattern.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(pattern.to_owned())
        },
    );
    matcher.insert("priority".to_owned(), serde_json::Value::from(priority));

    let recipe: Recipe =
        serde_json::from_value(value).map_err(|error| format!("流程 JSON 字段不合法：{error}"))?;
    let recipe = Recipe {
        id: forced_id.to_owned(),
        version: forced_version,
        ..recipe
    };
    recipe
        .validate()
        .map_err(|error| format!("流程校验失败：{error}"))?;
    Ok(recipe)
}

// ---------------------------------------------------------------------------
// 测试（纯函数；不渲染 GPUI）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rdm_resolve::recipe::SUPPORTED_OPS;
    use serde_json::json;

    const SAMPLE_RECIPE: &str = r#"{
        "schema": "rdm.recipe/v1",
        "id": "old-id",
        "version": 9,
        "name": "旧名称",
        "match": {
            "hosts": ["old.example.com"],
            "url_pattern": null,
            "priority": 0
        },
        "steps": [
            {
                "id": "req",
                "op": "http_request",
                "args": { "method": "GET", "url": "https://a.example.com/{{id}}" },
                "assign": { "body": "body_text" }
            }
        ],
        "emit": { "url": "https://cdn.example.com/{{id}}.bin" }
    }"#;

    // -- design 18 fixtures（纯 JSON 反序列化，不依赖磁盘/GPUI） ---------------

    /// 一段最小可读流程（ExperienceFlow）。
    fn flow_json() -> serde_json::Value {
        json!({
            "steps": [{
                "index": 0,
                "id": "req",
                "op": "http_request",
                "summary": "GET https://example.com/api/{{bvid}} （≤67108864 B）",
                "assignments": ["o0 ← body_json"],
                "expects": ["status ∈ [200, 206]"],
                "condition": null,
                "on_fail": null
            }],
            "emit": {
                "url": "https://cdn.example.com/{{bvid}}.mp4",
                "name": "{{bvid}}.mp4",
                "kind": "video",
                "one_time": false,
                "single_stream": false
            },
            "limits": {"requests": 8, "bytes_read": 1048576, "wall_ms": 15000, "redirects": 5},
            "captures": ["bvid"]
        })
    }

    /// 最小 `rdm.recipe/v1`（design 18 §2.1 的 generalization 可选）。
    fn recipe_json(
        id: &str,
        version: u32,
        leftover: &[&str],
        reusable: bool,
        include_generalization: bool,
    ) -> serde_json::Value {
        let mut value = json!({
            "schema": "rdm.recipe/v1",
            "id": id,
            "version": version,
            "name": format!("{id} 名称"),
            "match": {
                "hosts": ["example.com"],
                "url_pattern": "^https://example\\.com/v/(?P<bvid>BV[0-9A-Za-z]+)/?$",
                "priority": 0
            },
            "captures": ["bvid"],
            "steps": [{
                "id": "req",
                "op": "http_request",
                "args": {"method": "GET", "url": "https://example.com/api/{{bvid}}"}
            }],
            "emit": {"url": "https://cdn.example.com/{{bvid}}.mp4", "kind": "video"},
            "tests": [{"name": "sample", "input_url": "https://example.com/v/BV1xx"}]
        });
        if include_generalization {
            value["generalization"] = json!({
                "scope": "example.com/v",
                "captures": ["bvid"],
                "leftover_hardcoded": leftover,
                "reusable": reusable
            });
        }
        value
    }

    /// 构造一条 [`Experience`]（含 `flows` / `related` / 两个版本），用于纯函数测试。
    fn experience_fixture(
        id: &str,
        name: &str,
        created_ms: u64,
        related: &[&str],
        flow_count: usize,
        leftover: &[&str],
    ) -> Experience {
        let flow_views: Vec<serde_json::Value> = (0..flow_count)
            .map(|index| {
                let flow_id = if index == 0 {
                    id.to_owned()
                } else {
                    format!("{id}-sup{index}")
                };
                json!({
                    "recipe_id": flow_id,
                    "version": 1,
                    "primary": index == 0,
                    "name": if index == 0 {
                        "主流程".to_owned()
                    } else {
                        format!("补充流程 {index}")
                    },
                    "flow": flow_json(),
                    "recipe": recipe_json(&flow_id, 1, leftover, true, true),
                    "resources": 1
                })
            })
            .collect();
        let versions = json!([
            {
                "version": 1,
                "active": true,
                "health": {},
                "resources": 1,
                "created_ms": created_ms,
                "flow": flow_json(),
                "recipe": recipe_json(id, 1, leftover, true, true)
            },
            {
                "version": 2,
                "active": false,
                "health": {},
                "resources": 2,
                "created_ms": created_ms + 1,
                "flow": flow_json(),
                "recipe": recipe_json(id, 2, leftover, true, true)
            }
        ]);
        let value = json!({
            "id": id,
            "name": name,
            "description": String::new(),
            "hosts": ["example.com"],
            "url_pattern": "^https://example\\.com/v/(?P<bvid>BV[0-9A-Za-z]+)/?$",
            "priority": 0,
            "active_version": 1,
            "created_ms": created_ms,
            "updated_ms": created_ms,
            "use_count": 0,
            "success_count": 0,
            "failure_count": 0,
            "last_used_ms": null,
            "best_resources": 1,
            "last_resources": 0,
            "source_url": "https://example.com/v/BV1xx",
            "health": {},
            "flow": flow_json(),
            "versions": versions,
            "tests": [{"name": "sample", "input_url": "https://example.com/v/BV1xx"}],
            "recipe": recipe_json(id, 1, leftover, true, true),
            "resource_kinds": ["video"],
            "merged_into": null,
            "flows": flow_views,
            "related": related
        });
        serde_json::from_value(value).expect("experience fixture 反序列化失败")
    }

    fn recipe_from_value(value: &serde_json::Value) -> Recipe {
        Recipe::from_json(&serde_json::to_string(value).expect("json"))
            .expect("fixture recipe 应能通过解析/校验")
    }

    // -- design 18：列表丰富度 / 可复用规则 / 合并 / URL / 流程区 ------------

    #[test]
    fn richness_label_switches_to_flow_and_resource_counts_after_merge() {
        let single = experience_fixture("a", "A", 10, &[], 1, &[]);
        assert_eq!(list_richness_label(&single), "最佳 1 资源");
        let merged = experience_fixture("b", "B", 10, &[], 3, &[]);
        assert_eq!(list_richness_label(&merged), "3 个流程 · 1 个资源");
        let fallback = experience_fixture("c", "C", 10, &[], 0, &[]);
        assert_eq!(list_richness_label(&fallback), "最佳 1 资源");
        let mut unknown = single;
        unknown.best_resources = 0;
        assert_eq!(list_richness_label(&unknown), "资源数未知");
    }

    #[test]
    fn diagnose_scope_prefers_generalization_then_infers_legacy_recipes() {
        let reusable = recipe_from_value(&recipe_json("a", 1, &[], true, true));
        let diagnosis = diagnose_scope(&reusable);
        assert_eq!(diagnosis.policy, ScopePolicy::Reusable);
        assert_eq!(diagnosis.scope.as_deref(), Some("example.com/v"));
        assert!(diagnosis.leftover.is_empty());

        let hardcoded = recipe_from_value(&recipe_json("a", 1, &["cid"], false, true));
        let diagnosis = diagnose_scope(&hardcoded);
        assert_eq!(diagnosis.policy, ScopePolicy::Exact);
        assert_eq!(diagnosis.leftover, vec!["cid".to_owned()]);

        // 无 generalization（旧数据）：captures → 可复用；精确正则 → 仅精确地址；
        // 无 URL 正则 → 路径规则。
        let legacy = recipe_from_value(&recipe_json("a", 1, &[], true, false));
        assert_eq!(diagnose_scope(&legacy).policy, ScopePolicy::Reusable);
        let mut exact = recipe_json("a", 1, &[], true, false);
        exact["captures"] = json!([]);
        exact["match"]["url_pattern"] = json!("^https://example\\.com/v/BV[0-9A-Za-z]+$");
        assert_eq!(
            diagnose_scope(&recipe_from_value(&exact)).policy,
            ScopePolicy::Exact
        );
        let mut any_path = recipe_json("a", 1, &[], true, false);
        any_path["captures"] = json!([]);
        any_path["match"]["url_pattern"] = json!(null);
        assert_eq!(
            diagnose_scope(&recipe_from_value(&any_path)).policy,
            ScopePolicy::PathRule
        );
    }

    #[test]
    fn scope_text_marks_reusable_even_when_pattern_is_anchored() {
        // Lead 反馈的坑：v2 matcher 以 ^…$ 收尾但含 (?P<bvid>) + reusable=true，
        // 列表/详情必须显示「可复用规则」，不能沿用 v1 的「精确地址」。
        let experience = experience_fixture("e", "E", 1, &[], 1, &[]);
        assert_eq!(scope_text(&experience), "example.com · 可复用规则");

        let mut legacy = experience.clone();
        legacy.recipe.generalization = None;
        legacy.recipe.captures.clear();
        legacy.recipe.matcher.url_pattern =
            Some("^https://example\\.com/v/BV[0-9A-Za-z]+$".to_owned());
        assert_eq!(scope_text(&legacy), "example.com · 精确地址");

        let mut any_path = legacy;
        any_path.recipe.matcher.url_pattern = None;
        assert_eq!(scope_text(&any_path), "example.com · 任意路径");

        any_path.hosts.clear();
        assert_eq!(scope_text(&any_path), "任意主机 · 任意路径");
    }

    #[test]
    fn merge_plan_targets_earliest_created_experience() {
        let selected = experience_fixture("sel", "Sel", 300, &["a", "b"], 1, &[]);
        let earlier = experience_fixture("a", "A", 100, &[], 1, &[]);
        let middle = experience_fixture("b", "B", 200, &[], 1, &[]);
        let all = vec![selected.clone(), earlier, middle];
        let plan = choose_merge_plan(&selected, &all).expect("plan");
        assert_eq!(plan.target_id, "a");
        assert_eq!(
            plan.source_ids,
            vec!["b".to_owned(), "sel".to_owned()],
            "来源按创建时间升序，且包含当前选中经验"
        );
    }

    #[test]
    fn merge_plan_requires_two_visible_candidates() {
        let selected = experience_fixture("sel", "Sel", 300, &[], 1, &[]);
        assert!(choose_merge_plan(&selected, std::slice::from_ref(&selected)).is_none());
        let orphan = experience_fixture("sel", "Sel", 300, &["missing"], 1, &[]);
        assert!(choose_merge_plan(&orphan, std::slice::from_ref(&orphan)).is_none());
    }

    #[test]
    fn merge_scope_key_prefers_generalization_and_falls_back_to_hosts() {
        let experience = experience_fixture("e", "E", 1, &[], 1, &[]);
        assert_eq!(merge_scope_key(&experience), "scope:example.com/v");
        let mut legacy = experience;
        legacy.recipe.generalization = None;
        legacy.hosts = vec!["b.example.com".to_owned(), "a.example.com".to_owned()];
        legacy.url_pattern = Some("^https://a\\.example\\.com/v/(\\d+)$".to_owned());
        assert_eq!(
            merge_scope_key(&legacy),
            "hosts:a.example.com,b.example.com|pattern:^https://a\\.example\\.com/v/(\\d+)$"
        );
    }

    #[test]
    fn merge_kind_hint_lists_distinct_emit_kinds() {
        let experience = experience_fixture("e", "E", 1, &[], 2, &[]);
        assert_eq!(merge_kind_hint(&experience), "video");
    }

    #[test]
    fn merge_banner_text_mentions_count_and_complementary_kinds() {
        let text = merge_banner_text(2, "video / image");
        assert!(text.starts_with("发现 2 条同类经验"));
        assert!(text.contains("资源互补：video / image"));
        assert!(text.contains("合并后可一次获得全部资源"));
        let plain = merge_banner_text(1, "");
        assert!(plain.contains("1 条同类经验"));
        assert!(!plain.contains("资源互补"));
        assert!(merge_banner_text(0, "video").is_empty());
    }

    #[test]
    fn validate_experience_url_accepts_only_http_hostnames() {
        assert_eq!(
            validate_experience_url(" https://example.com/v/BV1xx ").expect("valid"),
            "https://example.com/v/BV1xx"
        );
        assert!(validate_experience_url("   ")
            .unwrap_err()
            .contains("请输入"));
        assert!(validate_experience_url("ftp://example.com/a")
            .unwrap_err()
            .contains("http/https"));
        assert!(validate_experience_url("example.com/a").is_err());
        assert!(validate_experience_url("https://").is_err());
    }

    #[test]
    fn flow_sections_mark_primary_and_supplement_and_fall_back() {
        let merged = experience_fixture("e", "E", 1, &[], 3, &[]);
        let sections = flow_sections(&merged, None);
        assert_eq!(sections.len(), 3);
        assert!(sections[0].primary);
        assert_eq!(sections[0].name, "主流程");
        assert_eq!(sections[0].recipe_id, "e");
        assert_eq!(sections[0].version, 1);
        assert!(!sections[1].primary);
        assert_eq!(sections[1].resources, 1);

        // core 旧数据没有 flows：回退到 experience.flow 作为唯一主流程。
        let fallback = experience_fixture("f", "F", 1, &[], 0, &[]);
        let sections = flow_sections(&fallback, None);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].primary);
        assert_eq!(sections[0].recipe_id, "f");
    }

    #[test]
    fn flow_sections_use_previewed_history_version() {
        let experience = experience_fixture("e", "E", 1, &[], 2, &[]);
        let history = &experience.versions[1];
        assert!(!history.active);
        let sections = flow_sections(&experience, Some(history));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].version, 2);
        assert_eq!(sections[0].name, "主流程");
    }

    // -- design 18：经验助手纯函数 -----------------------------------------

    #[test]
    fn assistant_append_keeps_tail_when_truncating() {
        assert_eq!(append_assistant_text("abc", "def", 4), "cdef");
        assert_eq!(append_assistant_text("abc", "def", 100), "abcdef");
        assert_eq!(append_assistant_text("", "中文", 2), "中文");
        // 4 个字符超出 3 字符上限：保留尾部 3 个（字符边界安全）。
        assert_eq!(append_assistant_text("中文", "字符", 3), "文字符");
    }

    #[test]
    fn assistant_messages_keep_only_the_most_recent() {
        let mut messages = Vec::new();
        for index in 0..(ASSISTANT_MESSAGES_MAX + 5) {
            push_assistant_message(&mut messages, AssistantMessage::user(format!("m{index}")));
        }
        assert_eq!(messages.len(), ASSISTANT_MESSAGES_MAX);
        assert_eq!(messages[0].text, "m5");
        assert_eq!(
            messages[ASSISTANT_MESSAGES_MAX - 1].text,
            format!("m{}", ASSISTANT_MESSAGES_MAX + 4)
        );
    }

    #[test]
    fn assistant_streaming_placeholder_is_visible_but_empty_user_message_is_not() {
        let streaming = AssistantMessage::assistant_streaming();
        assert!(streaming.is_visible());
        assert!(streaming.streaming);
        let empty_user = AssistantMessage::user("   ");
        assert!(!empty_user.is_visible());
        assert_eq!(empty_user.role, AssistantRole::User);
        let system = AssistantMessage::system("助手调用失败：网络错误");
        assert!(system.is_visible());
        assert_eq!(system.role, AssistantRole::System);
    }

    fn sample_form() -> EditFormValues {
        EditFormValues {
            name: "新名称".to_owned(),
            description: "新说明".to_owned(),
            hosts: "a.example.com, b.example.com，c.example.com;;\n d.example.com ".to_owned(),
            url_pattern: r"^https://a\.example\.com/(?P<id>\d+)$".to_owned(),
            priority: "7".to_owned(),
            json: SAMPLE_RECIPE.to_owned(),
        }
    }

    // -- 时间格式化 --------------------------------------------------------

    #[test]
    fn relative_time_covers_all_buckets() {
        let now = 1_000_000_000_000u64;
        assert_eq!(format_relative(now, None), "从未使用");
        assert_eq!(format_relative(now, Some(0)), "--");
        assert_eq!(format_relative(now, Some(now - 5_000)), "刚刚");
        assert_eq!(format_relative(now, Some(now + 5_000)), "刚刚");
        assert_eq!(format_relative(now, Some(now - 61_000)), "1 分钟前");
        assert_eq!(format_relative(now, Some(now - 3_600_000)), "1 小时前");
        assert_eq!(format_relative(now, Some(now - 90_000_000)), "1 天前");
        assert_eq!(format_relative(now, Some(now - 400 * 86_400_000)), "1 年前");
    }

    #[test]
    fn absolute_utc_time_is_stable() {
        assert_eq!(format_utc_datetime(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc_datetime(86_400_000), "1970-01-02 00:00 UTC");
        assert_eq!(
            format_utc_datetime(1_730_000_000_000),
            "2024-10-27 03:33 UTC"
        );
        assert_eq!(format_utc_date(1_730_000_000_000), "2024-10-27");
        assert_eq!(format_utc_date(1_704_067_200_000), "2024-01-01");
    }

    #[test]
    fn wall_time_format_is_readable() {
        assert_eq!(format_wall_ms(500), "500ms");
        assert_eq!(format_wall_ms(1_500), "1.5s");
        assert_eq!(format_wall_ms(2_000), "2s");
        assert_eq!(format_wall_ms(15_000), "15s");
    }

    // -- 搜索/过滤 ---------------------------------------------------------

    #[test]
    fn search_matches_name_host_and_url_case_insensitively() {
        assert!(search_matches_any("", &["example.com"]));
        assert!(search_matches_any("  ", &["example.com"]));
        assert!(search_matches_any("EXAMPLE", &["cdn.example.com"]));
        assert!(search_matches_any("示例", &["示例站点直链"]));
        assert!(!search_matches_any("missing", &["cdn.example.com", "示例"]));
        // 任一字段命中即可（主机 / 来源 URL / 说明混合场景）。
        assert!(search_matches_any(
            "download",
            &["名称", "example.com", "https://example.com/download/1"]
        ));
    }

    // -- 操作符标题 --------------------------------------------------------

    #[test]
    fn every_supported_op_has_a_chinese_title() {
        for op in SUPPORTED_OPS {
            let (title, _icon) = op_title(op);
            assert_ne!(title, "自定义步骤", "operator {op} 缺少中文标题");
        }
        assert_eq!(op_title("http_request").0, "请求网络");
        assert_eq!(op_title("json_query").0, "提取 JSON");
        assert_eq!(op_title("regex_capture").0, "正则提取");
        assert_eq!(op_title("url_join").0, "拼接地址");
        assert_eq!(op_title("template").0, "模板");
        assert_eq!(op_title("assert").0, "校验");
        assert_eq!(op_title("branch").0, "条件分支");
        assert_eq!(op_title("bounded_each").0, "遍历列表");
        assert_eq!(op_title("future_op").0, "自定义步骤");
    }

    // -- 编辑表单 → recipe -------------------------------------------------

    #[test]
    fn edit_json_applies_form_fields_and_forces_identity() {
        let recipe = recipe_from_edit_json(SAMPLE_RECIPE, &sample_form(), "exp-1", 4).unwrap();
        assert_eq!(recipe.id, "exp-1");
        assert_eq!(recipe.version, 4);
        assert_eq!(recipe.name, "新名称");
        assert_eq!(recipe.description, "新说明");
        assert_eq!(
            recipe.matcher.hosts,
            vec![
                "a.example.com",
                "b.example.com",
                "c.example.com",
                "d.example.com"
            ]
        );
        assert_eq!(
            recipe.matcher.url_pattern.as_deref(),
            Some(r"^https://a\.example\.com/(?P<id>\d+)$")
        );
        assert_eq!(recipe.matcher.priority, 7);
        assert!(recipe.validate().is_ok());
    }

    #[test]
    fn edit_json_empty_url_pattern_means_any_path() {
        let mut form = sample_form();
        form.url_pattern = "   ".to_owned();
        let recipe = recipe_from_edit_json(SAMPLE_RECIPE, &form, "exp-1", 2).unwrap();
        assert!(recipe.matcher.url_pattern.is_none());
    }

    #[test]
    fn edit_json_rejects_malformed_json() {
        let mut form = sample_form();
        form.json = "{ this is not json".to_owned();
        let error = recipe_from_edit_json(&form.json, &form, "exp-1", 2).unwrap_err();
        assert!(error.contains("JSON"), "unexpected error: {error}");
    }

    #[test]
    fn edit_json_rejects_non_object_root() {
        let mut form = sample_form();
        form.json = "[]".to_owned();
        let error = recipe_from_edit_json(&form.json, &form, "exp-1", 2).unwrap_err();
        assert!(error.contains("对象"), "unexpected error: {error}");
    }

    #[test]
    fn edit_json_rejects_bad_priority() {
        let mut form = sample_form();
        form.priority = "high".to_owned();
        let error = recipe_from_edit_json(SAMPLE_RECIPE, &form, "exp-1", 2).unwrap_err();
        assert!(error.contains("优先级"), "unexpected error: {error}");
    }

    #[test]
    fn edit_json_rejects_invalid_url_regex_via_validation() {
        let mut form = sample_form();
        form.url_pattern = "(".to_owned();
        let error = recipe_from_edit_json(SAMPLE_RECIPE, &form, "exp-1", 2).unwrap_err();
        assert!(error.contains("url_pattern"), "unexpected error: {error}");
    }

    #[test]
    fn edit_json_rejects_invalid_recipe_structure() {
        let mut form = sample_form();
        form.json = SAMPLE_RECIPE.replace("\"http_request\"", "\"eval_js\"");
        let error = recipe_from_edit_json(&form.json, &form, "exp-1", 2).unwrap_err();
        assert!(
            error.contains("operator") || error.contains("校验失败"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn hosts_parser_accepts_chinese_and_ascii_separators() {
        assert_eq!(
            parse_hosts(" a.com, b.com，c.com;;\n d.com \r\n"),
            vec!["a.com", "b.com", "c.com", "d.com"]
        );
        assert!(parse_hosts("  ,，; ").is_empty());
        assert_eq!(parse_hosts("example.com").len(), 1);
    }

    #[test]
    fn priority_parser_accepts_empty_and_integers() {
        assert_eq!(parse_priority(""), Ok(0));
        assert_eq!(parse_priority("   "), Ok(0));
        assert_eq!(parse_priority("7"), Ok(7));
        assert_eq!(parse_priority("-3"), Ok(-3));
        assert!(parse_priority("1.5").is_err());
    }
}
