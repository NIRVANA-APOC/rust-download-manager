//! RDM GUI (GPUI). Scaffold: verifies that GPUI builds; replaced by the real UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::{
    App, Application, Bounds, Context, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
};

struct RdmRoot {
    title: SharedString,
}

impl Render for RdmRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(0x101418))
            .size_full()
            .justify_center()
            .items_center()
            .text_xl()
            .text_color(rgb(0xe6edf3))
            .child(self.title.clone())
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(640.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| RdmRoot {
                    title: "RDM — Rust Download Manager".into(),
                })
            },
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
