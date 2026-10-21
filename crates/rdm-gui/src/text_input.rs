//! 单行文本输入框（URL / 设置项）。
//!
//! 参考 GPUI `examples/input.rs` 的完整实现，按 RDM 需要做了裁剪：
//! - 单行、无换行、无富文本；
//! - 焦点 + 键盘（Backspace/Delete/方向键/Home/End/全选/复制/粘贴/剪切）+ Enter 提交；
//! - 通过 `EntityInputHandler` 接平台输入法（IME）：marked text 会以下划线显示，
//!   `replace_and_mark_text_in_range` / `unmark_text` 都实现；中文输入法组合过程走这条路径。
//!
//! 已知限制（与 `docs/research/gpui.md` §6.2 一致）：
//! - 不引入 `unicode-segmentation`，光标按 `char` 边界移动（组合 emoji/复杂 grapheme 以
//!   字符为单位，而非字形簇）；
//! - 不支持 RTL / 双向文本 / 多行；
//! - IME 只在代码路径上接通，尚未在真实微软拼音/搜狗下逐键验证。

use std::ops::Range;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    div, fill, point, px, relative, size, Bounds, ClipboardItem, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ShapedLine, SharedString,
    Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};

use crate::theme::Palette;

gpui::actions!(
    rdm_text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Submit
    ]
);

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    palette: Palette,
    /// 掩码显示（API Key 等敏感字段）：内容仍按明文编辑/复制，绘制时每个字符显示为 `*`。
    masked: bool,
    /// 长文本水平滚动偏移（保证光标可见；由 prepaint 每帧按光标计算）。
    scroll_x: f32,
    /// Enter 回调（URL 框用于「添加」）。
    pub on_submit: Option<Rc<dyn Fn(&mut Window, &mut gpui::App)>>,
}

/// 掩码文本：每个字符一个 `*`。
///
/// 每个 `*` 恰好 1 字节（ASCII），因此「显示字节偏移 == 显示字符序号」，
/// 可以把内容里的 UTF-8 字节偏移映射到掩码文本的索引而不破坏光标/选区计算。
fn mask_text(content: &str) -> String {
    "*".repeat(content.chars().count())
}

/// 内容字节偏移 → 显示文本索引（未掩码时原样返回）。
fn display_offset(content: &str, content_offset: usize, masked: bool) -> usize {
    if !masked {
        return content_offset;
    }
    content
        .get(..content_offset.min(content.len()))
        .map(|prefix| prefix.chars().count())
        .unwrap_or(0)
}

/// 显示文本索引 → 内容字节偏移（未掩码时原样返回）。
fn content_offset(content: &str, display_index: usize, masked: bool) -> usize {
    if !masked {
        return display_index.min(content.len());
    }
    content
        .char_indices()
        .nth(display_index)
        .map(|(index, _)| index)
        .unwrap_or(content.len())
}

/// 计算让光标可见的水平滚动偏移（长路径/长 URL 在窄输入框里横向滚动）。
///
/// 纯函数便于单测：光标已在视口内 → 0；否则把光标推到右边缘留 2px 余量，
/// 且不超过文本末尾。
fn scroll_to_cursor(cursor_x: f32, text_width: f32, viewport_width: f32) -> f32 {
    if !cursor_x.is_finite()
        || !text_width.is_finite()
        || !viewport_width.is_finite()
        || viewport_width <= 0.0
    {
        return 0.0;
    }
    let margin = 2.0_f32.min(viewport_width * 0.5);
    let max_scroll = (text_width - viewport_width + margin).max(0.0);
    if cursor_x <= viewport_width - margin {
        0.0
    } else {
        (cursor_x - viewport_width + margin).clamp(0.0, max_scroll)
    }
}

impl TextInput {
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        palette: Palette,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            palette,
            masked: false,
            scroll_x: 0.0,
            on_submit: None,
        }
    }

    pub fn text(&self) -> &str {
        self.content.as_ref()
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// 切换掩码显示（只影响绘制，不改变内容）。
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    /// 内容字节偏移 → 当前显示文本的索引（掩码时按字符数折算）。
    fn display_offset(&self, content_offset: usize) -> usize {
        display_offset(&self.content, content_offset, self.masked)
    }

    fn content_offset_from_display(&self, display_index: usize) -> usize {
        content_offset(&self.content, display_index, self.masked)
    }

    /// 用代码设置内容（清空、预填目录等），光标移到末尾。
    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let len = self.content.len();
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let offset = self.previous_boundary(self.cursor_offset());
            self.move_to(offset, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let offset = self.next_boundary(self.selected_range.end);
            self.move_to(offset, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.previous_boundary(self.cursor_offset());
        self.select_to(offset, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.next_boundary(self.cursor_offset());
        self.select_to(offset, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.content.len();
        self.move_to(len, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let offset = self.previous_boundary(self.cursor_offset());
            self.select_to(offset, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let offset = self.next_boundary(self.selected_range.end);
            self.select_to(offset, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // 单行输入：换行折叠为空格，避免粘贴多行文本破坏布局。
            self.replace_text_in_range(None, &text.replace(['\r', '\n'], " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        // `on_submit` 回调常常要读取"提交这个输入框的同一个 Entity"（例如 URL 框的
        // on_submit → RdmApp::add_from_input 会 `url_input.read(cx)`，AI 聊天框会读
        // ai_chat_input）。如果同步调用，GPUI 会因该 Entity 仍处于 update 租约内而
        // double-lease panic（cannot read … while it is already being updated），
        // 而 GPUI 主线程是 non-unwinding panic → 进程直接闪退。
        //
        // 注意：**不能用 `cx.defer_in`**——它自身会 `view.update(...)` 重新更新本输入框，
        // 回调里再 read 同一个 Entity 依旧 double-lease。这里用 App 级 defer 捕获
        // WindowHandle，等本次 Entity 更新租约彻底释放后再进入窗口执行回调。
        if let Some(callback) = self.on_submit.clone() {
            let window_handle = window.window_handle();
            cx.defer(move |cx: &mut gpui::App| {
                let _ = window_handle.update(cx, |_, window, cx| callback(window, cx));
            });
        }
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        if event.modifiers.shift {
            let index = self.index_for_mouse_position(event.position);
            self.select_to(index, cx);
        } else {
            let index = self.index_for_mouse_position(event.position);
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_selecting {
            let index = self.index_for_mouse_position(event.position);
            self.select_to(index, cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + px(self.scroll_x))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// 上一个 char 边界（不引入 unicode-segmentation 的折中，见模块注释）。
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    /// 下一个 char 边界（不引入 unicode-segmentation 的折中，见模块注释）。
    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let scroll = px(self.scroll_x);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(self.display_offset(range.start)) - scroll,
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(self.display_offset(range.end)) - scroll,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let x = point.x - line_point.x + px(self.scroll_x);
        if self.masked {
            // 掩码文本每个字符都是 1 字节的 `*`：显示字节索引即显示字符序号。
            let display_index = last_layout.index_for_x(x)?;
            let content_offset = self.content_offset_from_display(display_index);
            return Some(self.offset_to_utf16(content_offset));
        }
        assert_eq!(last_layout.text, self.content);
        let utf8_index = last_layout.index_for_x(x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

struct TextElement {
    input: Entity<TextInput>,
    palette: Palette,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    /// 让光标可见的水平滚动偏移。
    scroll_x: f32,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let masked = input.masked;
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() {
            (
                input.placeholder.clone(),
                Hsla::from(self.palette.text_faint),
            )
        } else if masked {
            (mask_text(&content).into(), style.color)
        } else {
            (content.clone(), style.color)
        };
        let display_cursor = display_offset(&content, cursor, masked);
        let display_selection = display_offset(&content, selected_range.start, masked)
            ..display_offset(&content, selected_range.end, masked);
        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // 掩码文本每个字符都是 `*`，IME 的 marked range 无法安全映射，直接显示普通文本。
        let runs = match input.marked_range.as_ref() {
            Some(marked_range) if !masked => vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            _ => vec![run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_x: f32 = line.x_for_index(display_cursor).into();
        let text_width: f32 = line.width.into();
        let viewport_width: f32 = (bounds.right() - bounds.left()).into();
        let scroll_x = scroll_to_cursor(cursor_x, text_width, viewport_width);
        let shift = |x: f32| bounds.left() + px(x - scroll_x);
        let (selection, cursor) = if display_selection.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(shift(cursor_x), bounds.top()),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    self.palette.accent,
                )),
            )
        } else {
            let selection_start: f32 = line.x_for_index(display_selection.start).into();
            let selection_end: f32 = line.x_for_index(display_selection.end).into();
            (
                Some(fill(
                    Bounds::from_corners(
                        point(shift(selection_start), bounds.top()),
                        point(shift(selection_end), bounds.bottom()),
                    ),
                    self.palette.accent_soft,
                )),
                None,
            )
        };

        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            scroll_x,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let scroll_x = prepaint.scroll_x;
        let line = prepaint.line.take().expect("line was shaped in prepaint");
        line.paint(
            point(bounds.left() - px(scroll_x), bounds.top()),
            window.line_height(),
            window,
            cx,
        )
        .expect("paint text line");

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll_x = scroll_x;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let border = if focused {
            self.palette.accent
        } else {
            self.palette.border
        };
        let palette = self.palette;

        div()
            .flex()
            .items_center()
            .key_context("TextInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .h(px(36.))
            .px(px(10.))
            .bg(palette.panel_alt)
            .border_1()
            .border_color(border)
            .rounded_md()
            // 长文本（路径/URL）只做水平滚动，不撑破外层 flex 列。
            .overflow_hidden()
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(
                div()
                    .h(px(20.))
                    .w_full()
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .text_color(palette.text)
                    .child(TextElement {
                        input: cx.entity(),
                        palette,
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_text_replaces_each_char() {
        assert_eq!(mask_text(""), "");
        assert_eq!(mask_text("abc"), "***");
        // 每个 `*` 是 1 字节，因此掩码后的字节长度 == 字符数（光标映射的前提）。
        let masked = mask_text("密码a");
        assert_eq!(masked, "***");
        assert_eq!(masked.len(), 3);
    }

    #[test]
    fn display_offsets_round_trip_with_ascii() {
        let content = "abcd";
        for offset in 0..=content.len() {
            assert_eq!(display_offset(content, offset, true), offset);
            assert_eq!(content_offset(content, offset, true), offset);
        }
        // 未掩码时原样返回。
        assert_eq!(display_offset(content, 2, false), 2);
        assert_eq!(content_offset(content, 2, false), 2);
    }

    #[test]
    fn masked_offsets_respect_utf8_boundaries() {
        let content = "密码a"; // 3 + 3 + 1 字节
        assert_eq!(display_offset(content, 0, true), 0);
        assert_eq!(display_offset(content, 3, true), 1);
        assert_eq!(display_offset(content, 6, true), 2);
        assert_eq!(display_offset(content, 7, true), 3);
        assert_eq!(content_offset(content, 0, true), 0);
        assert_eq!(content_offset(content, 1, true), 3);
        assert_eq!(content_offset(content, 2, true), 6);
        assert_eq!(content_offset(content, 3, true), 7);
        // 越界输入不 panic，收敛到末尾。
        assert_eq!(content_offset(content, 99, true), 7);
        assert_eq!(display_offset(content, 99, true), 3);
    }

    #[test]
    fn scroll_keeps_cursor_visible_in_narrow_input() {
        // 文本比输入框短：不滚动。
        assert_eq!(scroll_to_cursor(50.0, 100.0, 200.0), 0.0);
        // 光标在视口内：不滚动。
        assert_eq!(scroll_to_cursor(150.0, 400.0, 200.0), 0.0);
        // 光标超出右边缘：滚到「光标 - 视口 + 余量」。
        assert_eq!(scroll_to_cursor(300.0, 400.0, 200.0), 102.0);
        // 光标在文本末尾：最多滚到「文本宽 - 视口 + 余量」。
        assert_eq!(scroll_to_cursor(400.0, 400.0, 200.0), 202.0);
        // 退化输入（宽度为 0/NaN）不 panic。
        assert_eq!(scroll_to_cursor(120.0, 300.0, 0.0), 0.0);
        assert_eq!(scroll_to_cursor(f32::NAN, 300.0, 100.0), 0.0);
        assert_eq!(scroll_to_cursor(10.0, f32::INFINITY, 100.0), 0.0);
    }
}
