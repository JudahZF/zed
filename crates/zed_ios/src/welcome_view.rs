//! Welcome view shown when the app launches.
//!
//! This is a placeholder that will be replaced with the connection UI in Phase 3.

#![allow(dead_code)]

use gpui::{Context, IntoElement, Render, SharedString, Window, div, prelude::*, px, rgb, rgba};

pub struct WelcomeView {
    title: SharedString,
    subtitle: SharedString,
    tap_count: usize,
}

impl WelcomeView {
    pub fn new() -> Self {
        Self {
            title: "Zed".into(),
            subtitle: "Code at the speed of thought".into(),
            tap_count: 0,
        }
    }
}

impl Default for WelcomeView {
    fn default() -> Self {
        Self::new()
    }
}
impl Render for WelcomeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Use explicit colors that will definitely be visible
        let bg_color = rgb(0x1e1e2e); // Dark background
        let accent_color = rgb(0x89b4fa); // Blue accent
        let text_color = rgb(0xcdd6f4); // Light text
        let muted_color = rgb(0xa6adc8); // Muted text
        let surface_color = rgb(0x313244); // Surface color
        let button_color = rgb(0x45475a); // Button background
        let button_hover = rgb(0x585b70); // Button hover

        // Format tap count for display
        let tap_text: SharedString = if self.tap_count == 0 {
            "Tap to test input".into()
        } else {
            format!(
                "Tapped {} time{}",
                self.tap_count,
                if self.tap_count == 1 { "" } else { "s" }
            )
            .into()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .justify_center()
            .items_center()
            .bg(bg_color)
            // Logo area - a simple colored rectangle as placeholder
            .child(
                div()
                    .w(px(120.0))
                    .h(px(120.0))
                    .rounded(px(24.0))
                    .bg(accent_color)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(48.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(bg_color)
                            .child("Z")
                    )
            )
            // Title
            .child(
                div()
                    .mt(px(32.0))
                    .text_size(px(42.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(text_color)
                    .child(self.title.clone()),
            )
            // Subtitle
            .child(
                div()
                    .mt(px(8.0))
                    .text_size(px(18.0))
                    .text_color(muted_color)
                    .child(self.subtitle.clone()),
            )
            // Interactive button to test touch input
            .child(
                div()
                    .id("tap-button")
                    .mt(px(32.0))
                    .px(px(32.0))
                    .py(px(16.0))
                    .rounded(px(12.0))
                    .bg(button_color)
                    .hover(|s| s.bg(button_hover))
                    .active(|s| s.bg(accent_color))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.tap_count += 1;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .text_size(px(18.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text_color)
                            .child(tap_text)
                    )
            )
            // Decorative card
            .child(
                div()
                    .mt(px(32.0))
                    .w(px(320.0))
                    .p(px(24.0))
                    .rounded(px(16.0))
                    .bg(surface_color)
                    .border_1()
                    .border_color(rgba(0xffffff20))
                    .flex()
                    .flex_col()
                    .gap(px(16.0))
                    // Status indicator
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .rounded(px(6.0))
                                    .bg(rgb(0xa6e3a1)) // Green dot
                            )
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .text_color(text_color)
                                    .child("Ready to connect")
                            )
                    )
                    // Info text
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(muted_color)
                            .child("Connect to a remote development server to start editing code on your iPad.")
                    )
            )
            // Version info at bottom
            .child(
                div()
                    .mt(px(48.0))
                    .text_size(px(12.0))
                    .text_color(rgba(0xffffff40))
                    .child("Zed for iPad - Preview")
            )
    }
}
