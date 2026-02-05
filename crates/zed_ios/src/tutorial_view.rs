//! Tutorial view for showing setup instructions.
//!
//! Displays a multi-step tutorial with navigation between steps.

#![allow(dead_code)]

use gpui::{
    div, prelude::*, px, rgb, App, Context, FocusHandle, Focusable, IntoElement, Render, Window,
};

/// The tutorial steps
const TUTORIAL_STEPS: &[TutorialStep] = &[
    TutorialStep {
        title: "Install Zed",
        content: include_str!("tutorial/01_install.md"),
    },
    TutorialStep {
        title: "Start Server",
        content: include_str!("tutorial/02_start_server.md"),
    },
    TutorialStep {
        title: "SSH Setup",
        content: include_str!("tutorial/03_ssh_setup.md"),
    },
    TutorialStep {
        title: "Connect",
        content: include_str!("tutorial/04_connect.md"),
    },
];

struct TutorialStep {
    title: &'static str,
    content: &'static str,
}

/// Event emitted when the tutorial is dismissed
pub struct TutorialDismissed;

/// Tutorial view with step navigation
pub struct TutorialView {
    current_step: usize,
    focus_handle: FocusHandle,
}

impl TutorialView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            current_step: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn go_to_step(&mut self, step: usize, cx: &mut Context<Self>) {
        if step < TUTORIAL_STEPS.len() {
            self.current_step = step;
            cx.notify();
        }
    }

    fn previous_step(&mut self, cx: &mut Context<Self>) {
        if self.current_step > 0 {
            self.current_step -= 1;
            cx.notify();
        }
    }

    fn next_step(&mut self, cx: &mut Context<Self>) {
        if self.current_step < TUTORIAL_STEPS.len() - 1 {
            self.current_step += 1;
            cx.notify();
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(TutorialDismissed);
    }

    fn render_step_indicators(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let accent_color = rgb(0x89b4fa);
        let muted_color = rgb(0x45475a);

        div()
            .flex()
            .gap(px(8.0))
            .justify_center()
            .children(TUTORIAL_STEPS.iter().enumerate().map(|(idx, _)| {
                let is_current = idx == self.current_step;
                let color = if is_current {
                    accent_color
                } else {
                    muted_color
                };

                div()
                    .id(format!("step-{}", idx))
                    .w(if is_current { px(24.0) } else { px(8.0) })
                    .h(px(8.0))
                    .rounded(px(4.0))
                    .bg(color)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event, _window, cx| {
                        this.go_to_step(idx, cx);
                    }))
            }))
    }

    fn render_content(&self, _cx: &App) -> impl IntoElement {
        let step = &TUTORIAL_STEPS[self.current_step];
        let text_color = rgb(0xcdd6f4);
        let muted_color = rgb(0xa6adc8);
        let code_bg = rgb(0x313244);
        let accent_color = rgb(0x89b4fa);

        let mut elements: Vec<gpui::AnyElement> = Vec::new();
        let mut in_code_block = false;

        for line in step.content.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }

            if in_code_block {
                elements.push(
                    div()
                        .px(px(12.0))
                        .py(px(4.0))
                        .bg(code_bg)
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(muted_color)
                                .child(line.to_string()),
                        )
                        .into_any_element(),
                );
                continue;
            }

            if trimmed.is_empty() {
                elements.push(div().h(px(12.0)).into_any_element());
            } else if trimmed.starts_with("# ") {
                elements.push(
                    div()
                        .text_size(px(28.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(text_color)
                        .mb(px(16.0))
                        .child(trimmed.trim_start_matches("# "))
                        .into_any_element(),
                );
            } else if trimmed.starts_with("## ") {
                elements.push(
                    div()
                        .text_size(px(22.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(text_color)
                        .mt(px(20.0))
                        .mb(px(12.0))
                        .child(trimmed.trim_start_matches("## "))
                        .into_any_element(),
                );
            } else if trimmed.starts_with("### ") {
                elements.push(
                    div()
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(text_color)
                        .mt(px(16.0))
                        .mb(px(8.0))
                        .child(trimmed.trim_start_matches("### "))
                        .into_any_element(),
                );
            } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                elements.push(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .mb(px(4.0))
                        .child(
                            div()
                                .text_size(px(16.0))
                                .text_color(accent_color)
                                .child("\u{2022}"),
                        )
                        .child(
                            div()
                                .text_size(px(16.0))
                                .text_color(text_color)
                                .flex_1()
                                .child(trimmed[2..].to_string()),
                        )
                        .into_any_element(),
                );
            } else if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit())
                && trimmed.contains(". ")
            {
                if let Some((num, rest)) = trimmed.split_once(". ") {
                    if num.chars().all(|c| c.is_ascii_digit()) {
                        elements.push(
                            div()
                                .flex()
                                .gap(px(8.0))
                                .mb(px(4.0))
                                .child(
                                    div()
                                        .text_size(px(16.0))
                                        .text_color(accent_color)
                                        .w(px(20.0))
                                        .child(format!("{}.", num)),
                                )
                                .child(
                                    div()
                                        .text_size(px(16.0))
                                        .text_color(text_color)
                                        .flex_1()
                                        .child(rest.to_string()),
                                )
                                .into_any_element(),
                        );
                        continue;
                    }
                }
                elements.push(
                    div()
                        .text_size(px(16.0))
                        .text_color(text_color)
                        .mb(px(8.0))
                        .line_height(px(24.0))
                        .child(trimmed.to_string())
                        .into_any_element(),
                );
            } else if trimmed.contains('`') {
                elements.push(
                    div()
                        .text_size(px(16.0))
                        .text_color(text_color)
                        .mb(px(8.0))
                        .child(trimmed.to_string())
                        .into_any_element(),
                );
            } else if trimmed
                .chars()
                .all(|c| c.is_ascii() && !c.is_alphanumeric() && c != ' ')
            {
                elements.push(
                    div()
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(6.0))
                        .bg(code_bg)
                        .mb(px(8.0))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .text_color(muted_color)
                                .child(trimmed.to_string()),
                        )
                        .into_any_element(),
                );
            } else {
                elements.push(
                    div()
                        .text_size(px(16.0))
                        .text_color(text_color)
                        .mb(px(8.0))
                        .line_height(px(24.0))
                        .child(trimmed.to_string())
                        .into_any_element(),
                );
            }
        }

        div().flex().flex_col().children(elements)
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button_color = rgb(0x45475a);
        let button_hover = rgb(0x585b70);
        let accent_color = rgb(0x89b4fa);
        let text_color = rgb(0xcdd6f4);
        let bg_color = rgb(0x1e1e2e);

        let is_first = self.current_step == 0;
        let is_last = self.current_step == TUTORIAL_STEPS.len() - 1;

        div()
            .flex()
            .gap(px(12.0))
            .w_full()
            .child(
                div()
                    .id("prev-button")
                    .flex_1()
                    .px(px(20.0))
                    .py(px(12.0))
                    .rounded(px(10.0))
                    .bg(button_color)
                    .when(!is_first, |el| el.hover(|s| s.bg(button_hover)))
                    .when(is_first, |el| el.opacity(0.5))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.previous_step(cx);
                    }))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text_color)
                            .text_center()
                            .child("Previous"),
                    ),
            )
            .child(
                div()
                    .id("next-button")
                    .flex_1()
                    .px(px(20.0))
                    .py(px(12.0))
                    .rounded(px(10.0))
                    .bg(if is_last { accent_color } else { button_color })
                    .hover(|s| s.bg(if is_last { rgb(0xa6c8ff) } else { button_hover }))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        if this.current_step == TUTORIAL_STEPS.len() - 1 {
                            this.dismiss(cx);
                        } else {
                            this.next_step(cx);
                        }
                    }))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if is_last { bg_color } else { text_color })
                            .text_center()
                            .child(if is_last { "Done" } else { "Next" }),
                    ),
            )
    }
}

impl gpui::EventEmitter<TutorialDismissed> for TutorialView {}

impl Focusable for TutorialView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TutorialView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_color = rgb(0x1e1e2e);
        let surface_color = rgb(0x313244);
        let muted_color = rgb(0xa6adc8);

        let step = &TUTORIAL_STEPS[self.current_step];

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(bg_color)
            .px(px(24.0))
            .py(px(24.0))
            .child(
                // Header with close button
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .mb(px(24.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(muted_color)
                            .child(format!(
                                "Step {} of {} - {}",
                                self.current_step + 1,
                                TUTORIAL_STEPS.len(),
                                step.title
                            )),
                    )
                    .child(
                        div()
                            .id("close-button")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(surface_color)
                            .hover(|s| s.bg(rgb(0x45475a)))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.dismiss(cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .text_color(muted_color)
                                    .child("Close"),
                            ),
                    ),
            )
            .child(
                // Step indicators
                div().mb(px(24.0)).child(self.render_step_indicators(cx)),
            )
            .child(
                // Content area
                div()
                    .id("tutorial-content")
                    .flex_1()
                    .overflow_y_scroll()
                    .p(px(16.0))
                    .rounded(px(12.0))
                    .bg(surface_color)
                    .child(self.render_content(cx)),
            )
            .child(
                // Navigation buttons
                div().mt(px(24.0)).child(self.render_navigation(cx)),
            )
    }
}
