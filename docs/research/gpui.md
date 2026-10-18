# GPUI 0.2.2 调研报告（RDM 桌面 GUI 技术选型）

> 状态：调研 + 选型依据 + Windows 落地坑位记录。对应任务 `task-3`。
>
> 版本基准：`gpui = "0.2.2"`（Zed Industries 发布到 crates.io），依赖声明见根 `Cargo.toml`：
> `gpui = { version = "0.2.2", default-features = false }`。
>
> 源码阅读基准：`%TEMP%\gpui-inspect\gpui-0.2.2`（crates.io 发布包解压，含 `src/`、`examples/`）。
> 本文所有 API 结论都能在该目录中找到对应文件；引用格式为 `文件:行号`。
>
> 结论先行：**GPUI 0.2.2 在本机 Windows 11 + Rust 1.99.1 GNU 工具链下可以编译并运行**，
> 但它是 pre-1.0、文档稀少的框架；我们通过「只用公开 prelude、锁定 0.2.2、核心与 GUI 解耦」
> 三条纪律控制风险。若未来需要迁移到 egui/iced，`rdm-core` 与 `rdm-ipc` 完全不用改。

---

## 1. GPUI 是什么

GPUI 是 Zed 编辑器背后的 Rust UI 框架，官方定位（`README.md:1-6`）：

> a hybrid immediate and retained mode, GPU accelerated, UI framework for Rust.

三层「语域」（`README.md:42-52`）：

| 层 | 类型 | 作用 |
| --- | --- | --- |
| 状态层 | `Entity<T>` + `App` | 应用状态全部由 `App` 持有，通过 `Entity` 句柄访问，类似 `Rc` 但由框架管理 |
| 视图层 | `Entity<T>` 实现 `Render` | 每帧调用 `render`，用 `div()` 等 element 构建树 |
| 元素层 | `Element` trait | 自定义布局/绘制的低层接口（`canvas`、`uniform_list` 都是这一层） |

关键设计点：

- **不是纯 immediate mode**：状态在 `Entity` 里持久化，`render` 只负责把当前状态映射成元素树；
  状态变化通过 `cx.notify()` 标记该 Entity 需要重绘（`src/app/context.rs:228-231`）。
- **retained 布局+GPU 绘制**：元素树交给 taffy 做 flex/grid 布局（`src/taffy.rs`，
  依赖固定 `taffy = "=0.9.0"`，见 `Cargo.toml:388-389`），再生成 scene 交给平台渲染器
  （Windows 是 DirectX 11，`src/platform/windows/directx_renderer.rs`；macOS Metal；Linux blade/Vulkan）。
- **平台窗口**：Windows 后端直接 Win32 + DirectComposition，没有 winit；这带来更原生的窗口行为，
  也带来更强的平台绑定（`src/platform/windows/window.rs`、`src/platform/windows/platform.rs`）。

版本事实：crate edition 是 **2024**（`gpui-0.2.2/Cargo.toml:13`），因此**要求较新的 stable Rust**
（edition 2024 最低 1.85；本机 1.99.1 满足）。我们的 workspace 仍是 edition 2021，依赖的 edition
互不影响。

---

## 2. Application / App / Context / Entity

### 2.1 程序入口

所有 GPUI 程序从 `Application` 开始（`examples/hello_world.rs:89-105`）：

```rust
Application::new().run(|cx: &mut App| {
    let bounds = Bounds::centered(None, size(px(500.), px(500.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_, cx| cx.new(|_| HelloWorld { text: "World".into() }),
    )
    .unwrap();
    cx.activate(true);
});
```

- `Application::new()` 初始化平台（窗口系统、渲染器、字体系统、剪贴板等）。
- `run(callback)` 进入平台事件循环，回调里创建窗口与根视图；回调返回后事件循环继续，
  直到 `cx.quit()` / 最后一个窗口关闭 / `cx.on_window_closed` 里主动退出。
- `Application::new().with_assets(Assets {})` 可以挂资产源（`examples/animation.rs:103-105`）。

### 2.2 Context 家族（`docs/contexts.md`）

| 类型 | 生命周期 | 用途 |
| --- | --- | --- |
| `App` | 平台事件循环内 `&mut` | 全局状态、打开窗口、创建 Entity、绑键、clipboard、executor |
| `Context<T>` | 事件回调内 `&mut` | 操作特定 Entity：`cx.notify()`、`cx.listener()`、`cx.spawn()`、`cx.entity()`；可 Deref 到 `App` |
| `AsyncApp` | `'static`，可跨 `await` | 异步任务里访问 App；方法返回 `Result`（App 可能已退出） |
| `AsyncWindowContext` | `'static` | 绑定窗口的异步上下文，可 `update(|window, cx| ...)` |
| `TestAppContext` | 测试 | `#[gpui::test]` 宏，模拟输入 |

`Context::spawn` 的签名很关键（`src/app/context.rs:233-245`）：

```rust
pub fn spawn<AsyncFn, R>(&self, f: AsyncFn) -> Task<R>
where
    T: 'static,
    AsyncFn: AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R + 'static,
    R: 'static;
```

即回调拿到 **弱引用 Entity** 和一个 `AsyncApp`，在 `await` 之后必须用
`this.update(&mut cx, |this, cx| ...)` 才能碰 UI 状态（Entity 可能已释放，返回 `Result`）。
返回值 `Task<R>` 必须持有或 `.detach()`，否则任务被取消。

### 2.3 Entity 与数据流

`Entity<T>` 是 `App` 内部 slot 的句柄（`src/app.rs` 的 `Entity`/`WeakEntity` 实现）：

- 创建：`cx.new(|cx| T { ... })`，闭包拿到 `&mut Context<T>`，可 `cx.focus_handle()`。
- 读取：`entity.read(cx)` / `entity.read_with(cx, |this, cx| ...)`。
- 修改：`entity.update(cx, |this, cx| { ...; cx.notify(); })`。
- 观察：`cx.observe(&other, |this, other, cx| ...)`、事件用 `EventEmitter` + `cx.subscribe`。
- 跨线程：不能跨线程访问 `Entity`；异步任务里保存 `WeakEntity`，在 GPUI foreground 任务里 update。

这一模型与 RDM 的需求非常契合：`RdmApp` 是唯一根 Entity；下载状态在 Entity 里，引擎事件从
tokio 侧汇入，主线程统一改状态并 `notify`。

---

## 3. Render / 视图生命周期

```rust
impl Render for RdmRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().bg(rgb(0x101418)).size_full()
            .child("Hello")
    }
}
```

- 根视图由 `cx.open_window(options, |window, cx| cx.new(...))` 创建；
  `WindowOptions` 定义在 `src/platform.rs:1087-1135`（`window_bounds`、`titlebar`、`focus`、
  `window_min_size`、`is_resizable`、`window_background` 等）。
- 重绘触发：`cx.notify()`、窗口 resize、输入事件、动画帧、`cx.refresh()`（AsyncApp）。
- 绘制顺序：`render` → request_layout（taffy）→ prepaint → paint；
  `canvas`、`ElementInputHandler`、`window.on_mouse_event` 等低层 API 都发生在 paint 阶段。
- `Render` 每帧都会重新执行 `render`；所以 `render` 内不要做重活（我们的行数据、
  速度历史都在 `Vec` 里直接读，sparkline 只 clone 60 个 f64）。

窗口关闭：`cx.on_window_closed(|cx| cx.quit()).detach()`（`examples/painting.rs:457-460`），
或 `cx.on_app_quit(...)` 做清理。注意 GPUI **不提供**「关闭窗口前拦截保存」的可靠钩子，
持久化应该随状态变化写（我们在 GUI 内做了 1s 节流的原子写）。

---

## 4. 元素与样式

### 4.1 div + Tailwind 风格

`div()` 是最常用的元素，样式方法由宏生成：`gpui_macros::style_helpers!()`（`src/styled.rs:25`），
所以有 `.flex()`、`.flex_col()`、`.gap_2()`、`.p_4()`、`.px_3()`、`.size_full()`、`.w_full()`、
`.bg(rgb(0x...))`、`.text_color(...)`、`.text_sm()`、`.rounded_md()`、`.border_1()`、
`.overflow_hidden()`、`.truncate()`、`.cursor_pointer()` 等；`hover()`/`active()` 接受
`StyleRefinement -> StyleRefinement` 闭包（`src/elements/div.rs:669-677`）。

常用示例可看 `examples/hello_world.rs:12-30`、`examples/data_table.rs:233-250`。

颜色/长度单位：`rgb(0xRRGGBB)`/`rgba(0xRRGGBBAA)`/`hsla(h,s,l,a)`；`px(12.)`、`relative(0.5)`、
`rems(1.5)`。`Background` 支持渐变（`examples/gradient.rs`、`examples/painting.rs:194-232`）。

### 4.2 字体与中文渲染（重要）

`Font` 结构在 `src/text_system.rs:785-804`，包含 `family`、`features`、`fallbacks`、`weight`、`style`。
`.font(Font { .. })` 会整套设置（`src/styled.rs:623-641`）。

GPUI 在 Windows 用 DirectWrite；**字体 fallback 只有显式配置时才会调用 `SetFontFallback`**
（`src/platform/windows/direct_write.rs:585-587`），fallback 的构造逻辑在
`direct_write.rs:324-377`：把用户列表里的字体按 Unicode range 映射，然后
`builder.AddMappings(&factory.GetSystemFontFallback()?)` 拼接系统 fallback。

因此 UI 里的中文不要赌「系统默认 fallback」：我们在根节点显式设置

```rust
let mut font = gpui::font("Segoe UI");
font.fallbacks = Some(FontFallbacks::from_fonts(vec![
    "Microsoft YaHei UI".into(),    // 中文
    "Segoe UI Symbol".into(),       // 符号
    "Segoe UI Emoji".into(),        // emoji（GPUI 有独立 emoji 光栅化 shader）
]));
```

文本样式还有 `.text_xs()/.text_sm()/.text_base()/.text_lg()/.text_xl()`（`src/styled.rs:424-490`）、
`.text_ellipsis()`、`.whitespace_nowrap()`、`.truncate()`（`src/styled.rs:81-125`）。
GPUI 支持彩色 emoji（`src/platform/windows/color_text_raster.hlsl`），但为了稳定性，
RDM 的界面文本使用中文 + ASCII 符号，不依赖 emoji 字形。

资产/图标：`AssetSource` + `svg()` 元素（`examples/animation.rs:10-34`、`examples/svg/svg.rs`），
`Application::with_assets` 注入。RDM v0.1 不打包图标资产，按钮使用文字标签，避免多一条资产链。

---

## 5. 布局（taffy）

- GPUI 用 taffy 0.9 提供 flexbox + grid（`src/taffy.rs`），有 `.flex_row()`、`.flex_col()`、
  `.justify_between()`、`.items_center()`、`.gap_4()`、`.flex_1()`、`.flex_shrink_0()`、
  `.absolute()`、`.top(px(..))`、`.relative()` 等。
- 尺寸：`.size(px(..))`、`.size_full()`、`.h_full()`、`.w(px(..))`、`.w(relative(..))`、
  `.min_w(..)`、`.max_w(..)`（由 style_helpers 生成）。
- 滚动：普通容器用 `.overflow_y_scroll()`（`src/elements/div.rs:1062`）；长列表用 `uniform_list`。
- 窗口尺寸：`WindowOptions.window_bounds` + `window_min_size`；
  `Bounds::centered(None, size(px(1180.), px(760.)), cx)`。
- 绝对定位叠加：设置弹窗用 `.absolute().top(px(0.)).left(px(0.))` 加全屏遮罩，
  这是 GPUI 里做 modal 的常规方式（没有内建 dialog 组件）。

### 5.1 uniform_list 虚拟列表（`src/elements/uniform_list.rs`）

API（`uniform_list.rs:22-55`）：

```rust
uniform_list(
    "entries",                       // ElementId
    item_count,
    cx.processor(|this, range: Range<usize>, window, cx| { /* 只构建可见行 */ }),
)
```

- **所有 item 必须等高**：它只测量第一个 item，然后按 `item_height * index` 定位
  （`uniform_list.rs:650-672` 的 `measure_item`）。我们的下载行固定 84px。
- 只渲染可见 range（`uniform_list.rs:450-493`），这是「万级下载列表」不卡的前提。
- 滚动条需要自己画：`track_scroll(UniformListScrollHandle)` 拿到偏移/内容高度，
  再叠加 thumb（`examples/data_table.rs:293-371` 是完整实现：canvas 里注册
  `window.on_mouse_event` 处理拖动）。RDM 用系统滚轮滚动（uniform_list 自带），
  不绘制自定义 thumb，减少代码面。
- 空列表：`item_count = 0` 时不渲染任何行，需要外面的空状态提示。
- 限制：高度必须统一；行内不能有展开/可变高度；`uniform_list` 不会自动滚动到新行。

---

## 6. 事件与输入

### 6.1 鼠标

- `div().on_mouse_down(MouseButton::Left, cx.listener(...))`、`on_mouse_up`、`on_mouse_move`、
  `on_click`（`src/elements/div.rs:484-491`；`ClickEvent` 是 mouse/keyboard 的合并枚举，
  `src/interactive.rs:164-267`）。
- `hover()` / `active()` 样式闭包；`.cursor_pointer()`、`.cursor(CursorStyle::IBeam)`。
- 事件回调通过 `cx.listener(|this, event, window, cx| ...)` 拿到 `&mut Self`。
- `window.on_mouse_event::<MouseDownEvent>(...)` 只在 paint 阶段注册，帧后清空
  （`src/window.rs:3416-3434`），data_table 的 scrollbar 就是这么做的。

### 6.2 键盘与 action

GPUI 的键盘处理是「keymap + action」：定义 `actions!(text_input, [Backspace, ...])`
（`examples/input.rs:13-31`），`cx.bind_keys([KeyBinding::new("backspace", Backspace, None)])`
（`examples/input.rs:680-694`），元素上 `.key_context("TextInput")` + `.on_action(cx.listener(...))`
（`examples/input.rs:567-588`）。`on_key_down` 也可用（`src/elements/div.rs:387-397`），
但 action 才能复用平台 keymap、处理修饰键。

文本输入框的完整实现见 `examples/input.rs`：自定义 `Element` + `ElementInputHandler`
（`src/input.rs:75-94`，`Window::handle_input` 在 `src/window.rs:3392-3414`），
实现 `EntityInputHandler` 的 8 个方法（`src/input.rs:10-73`）后，平台输入法（IME）
的 marked text 会走 `replace_and_mark_text_in_range`。RDM 的 URL 输入框就是按这个示例裁剪的
（见 `crates/rdm-gui/src/text_input.rs`），CJK IME 的 composition 用下划线标记。

**已知限制**：
- 示例依赖 `unicode-segmentation`；我们没有新增依赖，按 `char` 边界处理（组合 emoji/复杂
  grapheme 的光标移动会以字符为单位，不是字形簇）。
- 不支持双向文本（RTL）、富文本、换行输入。
- IME 的代码路径已接（marked range + 下划线 + `unmark_text`），但没有在真实微软拼音/搜狗
  输入法下逐键验证；冒烟测试只能验证 ASCII 键入。若 IME 异常，可在输入框里临时提供
  「粘贴 URL」替代路径（剪贴板操作不依赖 IME）。
- 没有 accessibility（屏幕阅读器）支持；GPUI 0.2.2 的无障碍能力非常有限。

### 6.3 焦点

`FocusHandle`（`src/window.rs:266-376`）：`cx.focus_handle()` 创建；`.track_focus(&handle)`
让元素接收焦点；`window.focus(&handle)` / `handle.focus(window)` 转移焦点；
`handle.is_focused(window)` 决定是否画光标；`window.focus_next()` 走 tab 顺序。
输入框的 paint 里必须 `window.handle_input(&focus_handle, ElementInputHandler::new(bounds, entity), cx)`。

### 6.4 剪贴板

`cx.read_from_clipboard() -> Option<ClipboardItem>`、`cx.write_to_clipboard(ClipboardItem::new_string(...))`
（`src/app.rs:1041-1055`），Windows 由 `src/platform/windows/clipboard.rs` 实现。

---

## 7. canvas 自定义绘制

```rust
canvas(
    |_bounds, _window, _cx| (),                    // prepaint
    |bounds, _state, window, _cx| {                // paint
        window.paint_quad(fill(
            Bounds::new(point(bounds.left(), bounds.top()), size(px(4.), px(10.))),
            rgb(0x3b82f6),
        ));
    },
).size_full()
```

- `canvas` 定义在 `src/elements/canvas.rs:10-19`：prepaint 返回状态，paint 消费状态。
- 低层画四边形：`window.paint_quad(PaintQuad)`（`src/window.rs:2839`）、`fill(bounds, background)`
  辅助函数（`src/window.rs:5078`）、`quad(...)`；画路径用 `window.paint_path(path, color)`
  （`examples/painting.rs:361-399`）。
- 路径/渐变/虚线：`PathBuilder`、`PathStyle::Stroke`、`linear_gradient`
  （`examples/painting.rs` 全篇）。
- RDM 的速度曲线用 `canvas` 画 60 个柱子：每个柱子一个 `fill`，宽度由 bounds 均分。
  这比 60 个 div 更省元素树，也避免每帧 60 个布局节点。

---

## 8. 动画

`Animation` + `AnimationExt::with_animation`（`examples/animation.rs:73-85`）：

```rust
svg().with_animation(
    "image_circle",
    Animation::new(Duration::from_secs(2)).repeat().with_easing(bounce(ease_in_out)),
    |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
)
```

- 动画需要 GPUI 请求动画帧（持续重绘），会占用 GPU/CPU。
- RDM v0.1 不做动画：下载进度的变化本身就是 250ms 级；设置弹窗直接显隐。
  这避免了 `Animation` 在窗口失焦/最小化时的额外重绘，也符合「无事件时 250ms 节流」。

---

## 9. 异步、执行器与 tokio 集成（本项目最关键设计）

### 9.1 GPUI 自己的执行器

- `App::background_executor()` / `App::foreground_executor()`（`src/app.rs:1401-1412`）。
- `cx.background_executor().spawn(async move { ... })`：后台线程池执行，返回 `Task<R>`。
- `cx.spawn(async move |weak_entity, async_cx| { ... })`：**主线程** future，可安全 update Entity。
- `Task::detach()` 让任务不随返回值析构而取消。
- `gpui::Timer`（`smol::Timer` 的重导出，`src/gpui.rs:94`）+ `cx.background_executor().timer(dur)`
  用于在 GPUI 执行器里做延时（不依赖 tokio reactor）。

### 9.2 为什么不能直接在 GPUI foreground future 里用 tokio 定时器

GPUI 的 foreground executor 是平台事件循环上的自定义执行器，**没有 tokio reactor 上下文**；
在它里面调用 `tokio::time::sleep` / `tokio::spawn` 会 panic（"no reactor running"）。
另一个方向（tokio 任务里操作 Entity）也不行：`Entity` 不是 `Send`。

所以架构是「双运行时 + 消息泵」：

```
  tokio Runtime（后台线程）
  ┌──────────────────────────────────────────────────────────┐
  │ download_with_handle(...)：rdm-core 引擎，多连接/限速/校验  │
  │      │ DownloadEvent (UnboundedSender)                     │
  │      ▼                                                     │
  │  per-download forward task ─┐                              │
  │  ticker task (250ms) ───────┼─▶ tokio mpsc::UnboundedChan  │
  │  IPC server tasks ──────────┘   (EngineMsg)                │
  └───────────────────────────────┬───────────────────────────┘
                                  │ recv().await
  GPUI foreground task（主线程）   ▼
  cx.spawn(async move |weak, cx| loop {
      let msg = rx.recv().await;                  // 跨运行时唤醒
      batch = drain_try_recv();                   // 合并突发事件
      weak.update(cx, |app, cx| { app.apply(batch); cx.notify(); })
      // 1s 节流持久化
  })
```

- 跨线程唤醒是安全的：GPUI 的 `ForegroundExecutor::spawn` 内部把 waker 交给平台
  dispatcher（Windows 上 `PostMessage`），tokio 侧 send 只是在 waker 上通知，
  不直接触碰 App 数据；真正的 `update` 发生在主线程。GPUI 自己的 HTTP/后台任务
  也是这个模式（`src/executor.rs` 的 `ForegroundExecutor`/`BackgroundExecutor`）。
- **250ms 节流**：引擎 reporter 本身 250ms 一帧（`docs/architecture.md` §3.5）；
  GUI 侧 ticker 也 250ms 一次，用于采样全局速度曲线并刷新 ETA。
  一批事件合并后只 `cx.notify()` 一次；空闲且无下载时 ticker 不触发重绘。
- **持久化节流**：状态变化标脏，最多 1s 落盘一次（原子写），
  引擎自身的 `.part.json` 才是断点的真相源，所以 GUI 侧丢 1s 的展示数据不致命。

这套桥接代码在 `crates/rdm-gui/src/engine.rs`。

### 9.3 与 rdm-core 的接口

- `rdm_core::download_with_handle(request, Some(tx), handle)` 在 tokio 任务里跑；
  `handle.pause()` / `handle.cancel()` 是跨任务的原子标志（`crates/rdm-core/src/lib.rs:177-212`）。
- `DownloadError::Paused { downloaded }` / `Cancelled { downloaded }` 是**控制流**，
  不是失败（`crates/rdm-core/src/lib.rs:224-226`、`src/error.rs:45-49`）。
  GUI 状态机必须把它们映射为 `Paused` / `Cancelled`，而不是 `Failed`。
- 恢复 = 用同一个 `DownloadRequest` 再调用一次；`.part` 由引擎接管。
- GUI 侧每个下载有一个 `run_id`，旧任务的迟到事件（暂停/恢复竞态）按 `run_id` 丢弃。

---

## 10. Windows 构建要求与坑

### 10.1 已在本机验证

- 环境：Windows 11，Rust 1.99.1，`x86_64-pc-windows-gnu`（MinGW-w64 UCRT）。
- `cargo build -p rdm-gui` 成功（首次约 4m25s，产出 `target/debug/rdm-gui.exe`；
  GPUI 及其约 600 个传递依赖全部编译通过）。
- **运行时**：窗口成功创建、D3D11 渲染管线出图（974x678 窗口，进程约 67MB），
  说明 debug 运行时 shader 编译 + DirectComposition 路径都正常。
- 配置：根 `Cargo.toml` 用 `default-features = false`，关闭 GPUI 默认的
  `wayland`/`x11`（Linux 后端）；但 **必须保留 `windows-manifest` feature**：

  ```toml
  gpui = { version = "0.2.2", default-features = false, features = ["windows-manifest"] }
  ```

  原因（实测踩坑，重要）：GPUI Windows 后端调用了 `TaskDialogIndirect`
  （comctl32 v6 才导出的 API）。关闭 `windows-manifest` 后 exe 没有声明
  comctl32 v6 的 side-by-side manifest，Win32 加载器按 comctl32 v5 解析导入，
  **进程启动即退出，退出码 `0xC0000139 STATUS_ENTRYPOINT_NOT_FOUND`**。
  保留该 feature 后 `embed-resource` 会把 GPUI 自带的 manifest/rc 编进 exe，
  启动正常。代价是：Windows GNU 工具链下 rustc 默认也会注入一个 manifest，
  `ld` 会报 `.rsrc merge failure: multiple non-default manifests`（不致命但噪音大）。
  **最终解法**：`crates/rdm-gui/build.rs` 里加 windows-gnu 专用 workaround——
  在 `OUT_DIR` 生成空的 `default-manifest.o`，通过
  `cargo:rustc-link-arg=-B<shadow-dir>` 覆盖 MinGW 自动链接的默认 manifest，
  让 GPUI 的 comctl32 v6 manifest 成为唯一资源。现在 `cargo build -p rdm-gui`
  无 ld 警告、双击可启动（Lead 已验证）。

### 10.2 Debug 构建：运行时编译 HLSL

**这是最大的隐藏坑**。Debug（`debug_assertions`）下 GPUI 不预编译 shader，而是启动时
用 `D3DCompileFromFile` 编译（`src/platform/windows/directx_renderer.rs:1380-1555`）：

```rust
let shader_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join(format!("src/platform/windows/{shader_name}"))
    .canonicalize()?;     // directx_renderer.rs:1519-1521
```

- 路径在**编译期**被写进二进制（`env!`），指向 cargo registry 里 gpui 源码目录，
  例如 `C:\Users\<user>\.cargo\registry\src\<hash>\gpui-0.2.2\src\platform\windows\shaders.hlsl`。
- 所以：`cargo clean` 只删 target 不影响；但如果清理了 **cargo registry 缓存**、把 exe
  拷到别的机器、或在 CI 容器里运行构建产物，debug exe 启动会因找不到 `.hlsl` 而渲染失败/panic。
- 依赖系统 `d3dcompiler_47.dll`（Windows 10/11 默认自带）。
- Release（`not(debug_assertions)`）由 `build.rs:248-433` 调 `fxc.exe` 预编译并 `include!`
  字节码；`fxc` 找不到时 `panic!("Failed to find fxc.exe")`，可用 `GPUI_FXC_PATH` 指定。
  因此 **release 构建必须有 Windows SDK 的 fxc**；本项目开发/CI 用 debug 规避
  （`docs/architecture.md` §8 也记录了这一点）。

### 10.3 其他坑

- GPUI 依赖 `windows = 0.61`，其 GNU 目标链接库由 `windows-targets` 提供；MinGW 工具链需完整
  （本机 WinLibs UCRT 可用）。PATH 里 mingw64\bin 的 `gcc`/`ld` 版本要和 rustc 的
  `x86_64-w64-mingw32-gcc` 兼容。
- 编译产物大：GPUI debug 依赖树约 600 crate，`target/debug` 体积数 GB；RDM 的
  `target-core/` 被 core-dev 单独用于不依赖 GPU 的 crate，两个 target 目录互不干扰
  （`CARGO_TARGET_DIR` 约定见 Lead 的说明）。
- `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`：release 无控制台；
  debug 有控制台，日志可见。RDM 在 debug 用 `tracing_subscriber` 输出到 stderr。
- 没有内建单实例：GPUI 不提供 `single-instance`；RDM 用自己的 IPC（`rdm-ipc`）实现，
  并在 `ipc.json` 里带 token，避免误连。
- 无内建菜单栏/对话框：设置弹窗是自绘 div；文件选择器（如果要）需要另找 `rfd` 之类依赖，
  RDM v0.1 用文本框填目录，不引入原生文件对话框。
- 字体：见 §4.2，必须显式配 CJK fallback，否则中文可能显示为方块（tofu）。
- 高刷/VSync：GPUI Windows 用 DirectComposition + DXGI；窗口最小化时刷新自动降低，
  不需要额外处理。
- 调试：`GPUI_LOG` / `RUST_LOG` 控制 GPUI 内部日志（GPUI 用 `log` crate）；debug 下
  `ID3D11Debug::ReportLiveDeviceObjects` 会在退出时打印泄漏对象（`directx_renderer.rs:1380-1387`），
  属正常噪音。

---

## 11. 与其他 GUI 方案对比

| 维度 | **GPUI 0.2.2** | egui | Tauri + Web | Electron | Slint | iced |
| --- | --- | --- | --- | --- | --- | --- |
| 范式 | retained Entity + 声明式元素 | immediate mode | Web 前端 + Rust 后端 | Chromium + JS | 声明式 DSL (.slint) | Elm 架构 |
| 渲染 | DirectX11/Metal/Vulkan（GPU） | wgpu/glow | 系统 WebView（WebView2） | Chromium 自带 | winit + femtovg/skia | wgpu |
| 纯 Rust | ✅（无 Web） | ✅ | ⚠️ 前端 JS/HTML | ❌ JS | ✅（DSL 编译器） | ✅ |
| 体积/依赖 | 大（~600 crate，debug 数 GB） | 小 | 中 + WebView2 运行时 | 大（>150MB） | 小 | 中 |
| 成熟度 | **pre-1.0**，API 变动频繁 | 高，0.3x 稳定 | 高 | 高 | 高（1.x） | 高（0.13+） |
| 文档/示例 | 少（靠 Zed 源码 + examples） | 多 | 多 | 极多 | 多 | 多 |
| 虚拟列表 | 内建 `uniform_list` | `ScrollArea::show_rows` | 前端方案 | 前端方案 | 需自研 | 需自研/`scrollable` |
| 自定义绘制 | `canvas` + scene 底层 API | `Painter` 很灵活 | Canvas/SVG | Canvas | 有限 | `canvas` widget |
| 中文/IME | DirectWrite，需配 fallback；IME 需实现 `EntityInputHandler` | 较好 | 浏览器原生 | 浏览器原生 | 依赖后端 | 较好 |
| 原生观感 | 好（Zed 同款） | 一般 | 取决于 Web | 非原生 | 好 | 中 |
| 许可 | Apache-2.0 | MIT/Apache | MIT/Apache | MIT | GPL/商业 | MIT |
| 与 tokio 集成 | 需自建 runtime + 消息泵（本文 §9） | 需 `request_repaint` | 天然（async command） | 天然（Node） | 需自建 | 需自建 |

**为什么选 GPUI（本项目约束下）**：

1. 架构文档 `docs/architecture.md` §1/§7 已冻结「GPUI，GPU 渲染，不依赖 Electron/CEF」；
   GUI 要展示多连接进度、速度曲线、万级列表，`uniform_list` + `canvas` 正好覆盖。
2. Zed 同款，实际产品验证过（Zed 是大型 IDE），Windows 后端 2025 年已可用。
3. 纯 Rust、Apache-2.0，无 WebView/Node 运行时，符合「纯 Rust 桌面下载器」的定位；
   Tauri 会引入 WebView2 依赖与前后端双栈，Electron 直接违背目标。
4. Entity 模型与「引擎事件泵入 UI」天然匹配：一个根 Entity 持有所有下载行，
   tokio 侧单向发消息，主线程统一变更，状态机好测（我们为纯逻辑写了单元测试）。

**pre-1.0 风险与规避**：

- 锁定精确版本 `=0.2.2`（workspace 里写 `0.2.2`，建议有 Cargo.lock 后固定）；
- 只用公开 prelude / 文档化 API，不碰 `private`、`scene` 内部结构；
- GUI 代码分层：`model.rs`（纯逻辑，可单测）、`engine.rs`（tokio 桥）、
  `ipc_server.rs`（协议）、`app.rs`（GPUI 视图）。换 UI 框架时只重写 `app.rs` + `text_input.rs`；
- debug 依赖 registry shader 路径的坑写进本文与 `docs/architecture.md`，CI 每次全新环境
  构建后跑冒烟；
- 若升级 GPUI，先改 `Cargo.toml` 跑 `cargo build -p rdm-gui` + 冒烟，再看 examples 是否编译。

---

## 12. RDM rdm-gui 的落地映射

| 需求 | GPUI 实现 | 代码位置 |
| --- | --- | --- |
| 1180x760 暗色三栏 | `WindowOptions.window_bounds` + flex 布局 | `src/main.rs`、`src/app.rs` |
| URL 单行输入（焦点/键盘/粘贴） | 自定义 Element + `EntityInputHandler` | `src/text_input.rs` |
| 虚拟列表 | `uniform_list` + 固定 84px 行高 | `src/app.rs` |
| 进度条/状态徽章 | div + `relative(frac)` 宽度 + 颜色 | `src/app.rs` |
| 速度曲线（60 采样） | `canvas` + `fill`/`paint_quad` 画柱 | `src/app.rs` |
| 设置弹窗 | absolute 遮罩 + 居中面板 | `src/app.rs` |
| 引擎事件泵 | tokio runtime + `mpsc` + `cx.spawn` 主线程泵 | `src/engine.rs` |
| IPC server | tokio `TcpListener` + JSONL + `rdm_ipc` 类型 | `src/ipc_server.rs` |
| 列表/设置持久化 | 原子写 JSON（`%LOCALAPPDATA%\RDM`） | `src/model.rs` |
| 单实例 / CLI URL | `rdm_ipc::is_gui_alive` + `send(Request::Enqueue)` | `src/main.rs` |

UI 状态机（下载行）：`Paused/Downloading/Completed/Failed/Cancelled`；
过滤「失败」包含 `Failed + Cancelled`；`DownloadError::Paused/Cancelled` 映射为控制流；
`run_id` 丢弃迟到事件。暂停/取消采用「两阶段」：GUI 先记录 `StopRequest::{Pause,Cancel}`，
状态继续保持 `Downloading`（徽章显示「暂停中/取消中」），直到引擎回报 `Paused/Cancelled`
才迁移状态 —— 避免用户在引擎尚未停稳时点「继续」，造成新旧任务同时写同一个 `.part` 文件。
单测覆盖格式化、过滤、状态迁移（含停止两阶段）、持久化 roundtrip、spool 解析、token 格式等纯逻辑
（`cargo test -p rdm-gui`）。

### 12.1 实现验证记录（2026-10-19，Windows 11 + Rust 1.99.1 GNU）

- `cargo build -p rdm-gui` 成功；`cargo test -p rdm-gui` **48/48 通过**。
- GPUI 运行时：窗口 1180x760（125% DPI 下 DWM 边界约 1477x989）正常出图；中文标签、
  uniform_list 虚拟列表、进度条、canvas 速度曲线、状态栏均正常。
- 真实下载联调（`rdm-fixture-server`，8MB/20ms 与 64MB/100ms）：
  - enqueue → downloading（约 4 MB/s，速度/ETA/连接数更新）→ pause（引擎确认后 `paused`，
    字节数冻结）→ resume → completed；
  - 8MB sha256 `68F564…03AD`；64MB 断点续传后 sha256 `E5A545…F60F`；`.part/.part.json` 零残留；
  - `rdm-gui.exe <URL>` 单实例投递（退出码 0、返回 download_id）并完成；
  - `downloads.json` 3 条 `completed`、`next_seq=4`。
  - 证据：`docs/evidence/rdm-gui-e2e-{downloading,paused,completed}.png` 与
    `docs/evidence/rdm-gui-e2e.json`（30 项断言全 PASS）。
- 已知限制：IME 未在真实输入法下逐键验证；SHA-256 开关只展示引擎返回的 hash；
  状态栏无「磁盘写入」数据源；`explorer /select` 只做了命令构造单测，未在自动化里真实弹窗。

---

## 13. 参考资料（本地解压源码）

- `gpui-0.2.2/README.md` — 定位与三层架构
- `gpui-0.2.2/docs/contexts.md` — App/Context/AsyncApp/Entity
- `gpui-0.2.2/src/gpui.rs` — crate 导出面（prelude、AppContext、VisualContext）
- `gpui-0.2.2/src/app.rs:1401-1430` — executor 与 `App::spawn`
- `gpui-0.2.2/src/app/context.rs:233-272` — `Context::spawn` / `listener` / `processor`
- `gpui-0.2.2/src/app/async_context.rs` — AsyncApp / AsyncWindowContext
- `gpui-0.2.2/src/elements/uniform_list.rs` — 虚拟列表实现与限制
- `gpui-0.2.2/src/elements/canvas.rs` — canvas prepaint/paint
- `gpui-0.2.2/src/styled.rs` — Tailwind 风格样式与宏
- `gpui-0.2.2/src/window.rs:266-376,3392-3414` — FocusHandle、`handle_input`
- `gpui-0.2.2/src/input.rs` — EntityInputHandler / ElementInputHandler
- `gpui-0.2.2/src/platform/windows/direct_write.rs:324-377,546-668` — DirectWrite fallback
- `gpui-0.2.2/src/platform/windows/directx_renderer.rs:1380-1555` — debug 运行时 shader 编译
- `gpui-0.2.2/build.rs:248-433` — release 的 fxc 预编译
- `gpui-0.2.2/examples/hello_world.rs`、`uniform_list.rs`、`input.rs`、`painting.rs`、
  `animation.rs`、`data_table.rs` — 本项目参考的六个示例
- 外部：<https://gpui.rs>、<https://github.com/zed-industries/zed>、
  <https://crates.io/crates/gpui/0.2.2>

---

## 14. 结论

GPUI 0.2.2 能在 Windows + GNU 工具链下工作，足够支撑 RDM v0.1 的三栏列表、自绘进度条、
速度曲线与设置弹窗；代价是 pre-1.0 的 API 风险、稀疏文档、debug 运行时 shader 编译的环境依赖、
以及需要自己搭 tokio↔GUI 的消息泵。我们通过锁版本、只使用公开 API、逻辑层与视图层分离、
把风险点写入文档与 CI 冒烟来对冲。若后续 GPUI 升级或迁移到其他框架，`rdm-core` 与
`rdm-ipc` 不受影响，迁移成本集中在 `crates/rdm-gui/src/{app,text_input}.rs`。
