//! Workspace view shown after successful SSH connection.
//!
//! This is a placeholder that will eventually become the full remote editing workspace.

use gpui::{
    div, prelude::*, px, rgb, rgba, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, Window,
};
use remote::RemoteClient;

/// Event emitted when user wants to disconnect
pub struct DisconnectRequested;

/// Workspace view for remote editing
pub struct WorkspaceView {
    #[allow(dead_code)]
    client: Entity<RemoteClient>,
    focus_handle: FocusHandle,
    status_message: String,
}

impl WorkspaceView {
    pub fn new(client: Entity<RemoteClient>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            client,
            focus_handle: cx.focus_handle(),
            status_message: "Connected to remote server".to_string(),
        }
    }

    fn disconnect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Disconnect requested");
        cx.emit(DisconnectRequested);
    }

    fn render_header(&self, _cx: &App) -> impl IntoElement {
        let accent_color = rgb(0xa6e3a1); // Green for connected
        let bg_color = rgb(0x1e1e2e);
        let text_color = rgb(0xcdd6f4);
        let muted_color = rgb(0xa6adc8);

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(80.0))
                    .h(px(80.0))
                    .rounded(px(16.0))
                    .bg(accent_color)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(36.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(bg_color)
                            .child("Z"),
                    ),
            )
            .child(
                div()
                    .text_size(px(28.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(text_color)
                    .child("Connected"),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(muted_color)
                    .child(self.status_message.clone()),
            )
    }

    fn render_placeholder(&self, _cx: &App) -> impl IntoElement {
        let surface_color = rgb(0x313244);
        let text_color = rgb(0xcdd6f4);
        let muted_color = rgb(0xa6adc8);

        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .w_full()
            .max_w(px(500.0))
            .p(px(24.0))
            .rounded(px(12.0))
            .bg(surface_color)
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(text_color)
                    .child("Remote Workspace"),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(muted_color)
                    .child("You are connected to the remote server."),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(muted_color)
                    .child("The full editing workspace is coming soon. For now, you can:"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .pl(px(16.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(muted_color)
                            .child("• Verify the connection is working"),
                    )
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(muted_color)
                            .child("• Disconnect and try other servers"),
                    ),
            )
    }

    fn render_disconnect_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button_color = rgb(0xf38ba8); // Red for disconnect
        let button_hover = rgb(0xf5a0b8);
        let bg_color = rgb(0x1e1e2e);

        div()
            .id("disconnect-button")
            .w_full()
            .max_w(px(400.0))
            .px(px(24.0))
            .py(px(14.0))
            .rounded(px(10.0))
            .bg(button_color)
            .hover(|s| s.bg(button_hover))
            .cursor_pointer()
            .on_click(cx.listener(|this, _event, window, cx| {
                this.disconnect(window, cx);
            }))
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(bg_color)
                    .text_center()
                    .child("Disconnect"),
            )
    }
}

impl EventEmitter<DisconnectRequested> for WorkspaceView {}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_color = rgb(0x1e1e2e);

        div()
            .id("workspace-view")
            .flex()
            .flex_col()
            .size_full()
            .bg(bg_color)
            .p(px(24.0))
            .items_center()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(32.0))
                    .py(px(48.0))
                    .w_full()
                    .child(self.render_header(cx))
                    .child(self.render_placeholder(cx))
                    .child(self.render_disconnect_button(cx)),
            )
            .child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .left_0()
                    .right_0()
                    .text_center()
                    .text_size(px(12.0))
                    .text_color(rgba(0xffffff40))
                    .child("Zed for iPad - Preview"),
            )
    }
}
