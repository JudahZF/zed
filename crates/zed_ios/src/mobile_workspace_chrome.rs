use std::sync::Arc;

use agent_ui::AgentPanel;
use diagnostics::Deploy as DeployDiagnostics;
use git_ui::git_panel::GitPanel;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, px, rgb,
};
use outline_panel::OutlinePanel;
use project::trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent};
use project_panel::ProjectPanel;
use remote::RemoteClient;
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Button, ButtonStyle, Color, Icon, IconName, IconSize, LabelSize, TintColor, prelude::*};
use workspace::dock::{DockPosition, Panel};
use workspace::{AppState, Workspace};
use zed_actions::{Spawn, command_palette};

use crate::mobile_feature_policy::MobileFeaturePolicy;
use crate::persistence::{
    ConnectionDb, ConnectionProfile, MobileWorkspaceSnapshotV1, SessionRestoreState,
    current_timestamp,
};
use crate::port_forward_manager::{PortForwardManager, PortForwardState};
use crate::root_view::RootView;
use crate::session_recovery::{
    SessionLifecyclePhase, SessionRecoveryCoordinator, SessionRecoveryState,
};

pub struct MobileWorkspaceChrome {
    workspace: Entity<Workspace>,
    app_state: Arc<AppState>,
    connection_profile_id: Option<i64>,
    remote_path: Option<String>,
    recovery_coordinator: Entity<SessionRecoveryCoordinator>,
    port_forward_manager: Entity<PortForwardManager>,
    active_connection_profile: Option<ConnectionProfile>,
    feature_policy: MobileFeaturePolicy,
    selected_tool: Option<String>,
}

impl MobileWorkspaceChrome {
    pub fn new(
        workspace: Entity<Workspace>,
        remote_client: Entity<RemoteClient>,
        connection_profile_id: Option<i64>,
        remote_path: Option<String>,
        app_state: Arc<AppState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_connection_profile = connection_profile_id.and_then(|id| {
            ConnectionDb::open()
                .ok()?
                .connection_profile_by_id(id)
                .ok()?
        });
        let feature_policy = MobileFeaturePolicy::from_app(cx);
        let desired_forwards = active_connection_profile
            .as_ref()
            .map(|profile| profile.port_forwards.clone())
            .unwrap_or_default();
        let recovery_coordinator = SessionRecoveryCoordinator::global(cx)
            .expect("SessionRecoveryCoordinator must be initialized before opening iOS workspace");
        SessionRecoveryCoordinator::install_session(
            remote_client.clone(),
            connection_profile_id,
            remote_path.clone(),
            cx,
        );

        let port_forward_manager = cx.new(|_| {
            PortForwardManager::new(
                remote_client.clone(),
                connection_profile_id,
                desired_forwards,
            )
        });

        let mut this = Self {
            workspace: workspace.clone(),
            app_state,
            connection_profile_id,
            remote_path,
            recovery_coordinator,
            port_forward_manager,
            active_connection_profile,
            feature_policy,
            selected_tool: Some("files".to_string()),
        };

        cx.observe(&this.recovery_coordinator, |this, _, cx| {
            this.sync_runtime_state(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&this.port_forward_manager, |_, _, cx| {
            cx.notify();
        })
        .detach();
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            cx.subscribe(&trusted_worktrees, |_, _, _: &TrustedWorktreesEvent, cx| {
                cx.notify();
            })
            .detach();
        }

        this.sync_runtime_state(cx);
        this
    }

    fn sync_runtime_state(&mut self, cx: &mut Context<Self>) {
        let lifecycle_phase = self.recovery_coordinator.read(cx).lifecycle_phase();
        let recovery_state = self.recovery_coordinator.read(cx).state();
        let should_suspend_forwards = !matches!(lifecycle_phase, SessionLifecyclePhase::Active)
            || recovery_state != SessionRecoveryState::Healthy;

        self.port_forward_manager.update(cx, |manager, cx| {
            if manager.is_suspended() != should_suspend_forwards {
                manager.set_suspended(should_suspend_forwards, cx);
                return;
            }

            if !should_suspend_forwards
                && manager.has_forwards()
                && manager
                    .statuses()
                    .iter()
                    .all(|status| status.state == PortForwardState::Pending)
            {
                manager.restart(cx);
            }
        });
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = ConnectionDb::open().and_then(|db| db.clear_session_restore_state()) {
            log::warn!("[Zed iOS] Failed to clear restore state during disconnect: {err}");
        }
        SessionRecoveryCoordinator::clear_session(cx);
        let app_state = self.app_state.clone();
        window.replace_root(cx, move |window, cx| {
            RootView::new_without_auto_restore(app_state, window, cx)
        });
    }

    fn active_recovery_state(&self, cx: &App) -> SessionRecoveryState {
        self.recovery_coordinator.read(cx).state()
    }

    fn session_label(&self) -> SharedString {
        self.active_connection_profile
            .as_ref()
            .map(|profile| {
                profile
                    .nickname
                    .clone()
                    .unwrap_or_else(|| profile.hostname.clone())
            })
            .unwrap_or_else(|| "Remote session".to_string())
            .into()
    }

    fn recovery_status_label(state: SessionRecoveryState) -> &'static str {
        match state {
            SessionRecoveryState::Healthy => "Connected",
            SessionRecoveryState::Reconnecting => "Reconnecting",
            SessionRecoveryState::ReconnectFailedRetryable => "Reconnect failed",
            SessionRecoveryState::ReconnectExhausted => "Disconnected",
        }
    }

    fn persist_workspace_snapshot(&self, cx: &mut Context<Self>) {
        let Some(connection_profile_id) = self.connection_profile_id else {
            return;
        };

        let remote_path = self.remote_path.clone().or_else(|| {
            self.active_connection_profile
                .as_ref()
                .and_then(|profile| profile.default_path.clone())
        });
        let snapshot = self.workspace.read_with(cx, |workspace, cx| {
            let left_sidebar_visible = workspace
                .dock_at_position(DockPosition::Left)
                .read(cx)
                .is_open();

            let right_dock = workspace.dock_at_position(DockPosition::Right).read(cx);
            let right_panel_key = right_dock.active_panel().map(|panel| panel.panel_key());
            let right_dock_open = right_dock.is_open();

            let bottom_dock = workspace.dock_at_position(DockPosition::Bottom).read(cx);
            let bottom_panel_key = bottom_dock.active_panel().map(|panel| panel.panel_key());
            let bottom_dock_open = bottom_dock.is_open();

            let mut snapshot =
                MobileWorkspaceSnapshotV1::new(remote_path.clone(), remote_path.clone());
            snapshot.active_tool = self.selected_tool.clone();
            snapshot.left_sidebar_visible = left_sidebar_visible;
            snapshot.terminal_visible =
                bottom_dock_open && bottom_panel_key == Some(TerminalPanel::panel_key());
            snapshot.git_panel_visible =
                right_dock_open && right_panel_key == Some(GitPanel::panel_key());
            snapshot.agent_visible =
                right_dock_open && right_panel_key == Some(AgentPanel::panel_key());
            snapshot
        });

        let snapshot_json = match snapshot.to_json() {
            Ok(snapshot_json) => snapshot_json,
            Err(err) => {
                log::warn!("[Zed iOS] Failed to serialize workspace snapshot: {err}");
                return;
            }
        };

        if let Err(err) = ConnectionDb::open().and_then(|db| {
            db.save_session_restore_state(&SessionRestoreState {
                connection_profile_id,
                remote_path: remote_path.clone(),
                last_opened_worktree: remote_path.clone(),
                workspace_state_json: Some(snapshot_json),
                updated_at: current_timestamp(),
            })
        }) {
            log::warn!("[Zed iOS] Failed to persist workspace snapshot: {err}");
        }
    }

    fn disconnect_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("disconnect-button")
            .px(px(12.0))
            .py(px(8.0))
            .min_h(px(36.0))
            .rounded(px(10.0))
            .bg(rgb(0x232a36))
            .hover(|style| style.bg(rgb(0x2d3644)))
            .cursor_pointer()
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(0xe2e8f0))
                    .child("Disconnect"),
            )
            .on_click(cx.listener(|this, _event, window, cx| this.disconnect(window, cx)))
    }

    fn tool_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let compact = self.feature_policy.compact_panels;
        div()
            .id(id)
            .px(if compact { px(10.0) } else { px(12.0) })
            .py(if compact { px(8.0) } else { px(9.0) })
            .rounded(px(10.0))
            .bg(rgb(0x111827))
            .hover(|style| style.bg(rgb(0x1f2937)))
            .cursor_pointer()
            .child(
                div()
                    .text_size(if compact { px(12.0) } else { px(13.0) })
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(rgb(0xe2e8f0))
                    .child(label),
            )
            .on_click(cx.listener(move |this, _event, window, cx| on_click(this, window, cx)))
    }

    fn toggle_project_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel_focus::<ProjectPanel>(window, cx);
        });
        self.selected_tool = Some("files".to_string());
        self.persist_workspace_snapshot(cx);
    }

    fn toggle_outline_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel_focus::<OutlinePanel>(window, cx);
        });
        self.selected_tool = Some("outline".to_string());
        self.persist_workspace_snapshot(cx);
    }

    fn toggle_git_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel_focus::<GitPanel>(window, cx);
        });
        self.selected_tool = Some("git".to_string());
        self.persist_workspace_snapshot(cx);
    }

    fn toggle_terminal_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
        });
        self.selected_tool = Some("terminal".to_string());
        self.persist_workspace_snapshot(cx);
    }

    fn toggle_agent_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.toggle_panel_focus::<AgentPanel>(window, cx);
        });
        self.selected_tool = Some("agent".to_string());
        self.persist_workspace_snapshot(cx);
    }

    fn open_project_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(workspace::DeploySearch::find()), cx);
    }

    fn open_tasks_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(Spawn::modal()), cx);
    }

    fn open_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(DeployDiagnostics), cx);
    }

    fn open_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(workspace::ToggleFileFinder::default()), cx);
    }

    fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(command_palette::Toggle), cx);
    }

    fn has_restricted_worktrees(&self, cx: &App) -> bool {
        TrustedWorktrees::try_get_global(cx)
            .map(|trusted_worktrees| {
                let project = self.workspace.read(cx).project().read(cx).worktree_store();
                trusted_worktrees
                    .read(cx)
                    .has_restricted_worktrees(&project, cx)
            })
            .unwrap_or(false)
    }

    fn open_restricted_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.show_worktree_trust_security_modal(true, window, cx);
        });
    }

    fn restricted_mode_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("restricted-mode-button", "Restricted Mode")
            .style(ButtonStyle::Tinted(TintColor::Warning))
            .label_size(LabelSize::Small)
            .color(Color::Warning)
            .start_icon(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_restricted_mode(window, cx);
            }))
    }

    fn retry_port_forwards(&mut self, cx: &mut Context<Self>) {
        self.port_forward_manager.update(cx, |manager, cx| {
            manager.restart(cx);
        });
    }

    fn format_port_forward_status(
        &self,
        state: &str,
        count: usize,
        sample_specs: &[String],
    ) -> String {
        let mut label = format!("Ports: {count} {state}");
        if !sample_specs.is_empty() {
            label.push_str(" (");
            label.push_str(&sample_specs.join(", "));
            label.push(')');
        }
        label
    }

    fn render_port_forward_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (active_count, pending_count, failed_count, sample_specs, last_error) = {
            let manager = self.port_forward_manager.read(cx);
            let statuses = manager.statuses();
            let active_count = statuses
                .iter()
                .filter(|status| status.state == PortForwardState::Active)
                .count();
            let pending_count = statuses
                .iter()
                .filter(|status| status.state == PortForwardState::Pending)
                .count();
            let failed_count = statuses
                .iter()
                .filter(|status| status.state == PortForwardState::Failed)
                .count();
            let sample_specs = statuses
                .iter()
                .take(2)
                .map(|status| {
                    let local_host = status
                        .spec
                        .local_host
                        .clone()
                        .unwrap_or_else(|| "127.0.0.1".to_string());
                    let remote_host = status
                        .spec
                        .remote_host
                        .clone()
                        .unwrap_or_else(|| "localhost".to_string());
                    format!(
                        "{}:{} -> {}:{}",
                        local_host, status.spec.local_port, remote_host, status.spec.remote_port
                    )
                })
                .collect::<Vec<_>>();
            let last_error = manager.last_error().map(ToOwned::to_owned);

            (
                active_count,
                pending_count,
                failed_count,
                sample_specs,
                last_error,
            )
        };
        let status_label = if failed_count > 0 {
            self.format_port_forward_status("failed", failed_count, &sample_specs)
        } else if pending_count > 0 {
            self.format_port_forward_status("pending", pending_count, &sample_specs)
        } else {
            self.format_port_forward_status("active", active_count, &sample_specs)
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .w_full()
            .px(px(16.0))
            .py(px(10.0))
            .bg(rgb(0x08111f))
            .border_b_1()
            .border_color(rgb(0x111827))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xcbd5e1))
                            .truncate()
                            .child(status_label),
                    )
                    .when_some(last_error, |el, error| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(0xfca5a5))
                                .truncate()
                                .child(error),
                        )
                    }),
            )
            .when(failed_count > 0, |el| {
                el.child(
                    div()
                        .id("retry-port-forwards-button")
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(10.0))
                        .bg(rgb(0x1d4ed8))
                        .hover(|style| style.bg(rgb(0x2563eb)))
                        .cursor_pointer()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(0xf8fafc))
                                .child("Retry"),
                        )
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.retry_port_forwards(cx);
                        })),
                )
            })
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status = Self::recovery_status_label(self.active_recovery_state(cx));

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .w_full()
            .min_h(px(52.0))
            .px(px(16.0))
            .py(px(10.0))
            .bg(rgb(0x0f172a))
            .border_b_1()
            .border_color(rgb(0x1f2937))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(0xe2e8f0))
                            .truncate()
                            .child(self.session_label()),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(0x94a3b8))
                            .child(status),
                    ),
            )
            .child(self.disconnect_button(cx))
    }

    fn render_tool_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = self.feature_policy.compact_panels;
        let agent_label = if self.feature_policy.mobile_agent_review {
            "Review"
        } else {
            "Agent"
        };

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(if compact { px(8.0) } else { px(10.0) })
            .w_full()
            .px(px(16.0))
            .py(px(12.0))
            .bg(rgb(0x020617))
            .border_b_1()
            .border_color(rgb(0x111827))
            .when(self.has_restricted_worktrees(cx), |el| {
                el.child(self.restricted_mode_button(cx))
            })
            .child(
                self.tool_button("tool-files", "Files", cx, |this, window, cx| {
                    this.toggle_project_panel(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-outline", "Outline", cx, |this, window, cx| {
                    this.toggle_outline_panel(window, cx);
                }),
            )
            .child(self.tool_button("tool-git", "Git", cx, |this, window, cx| {
                this.toggle_git_panel(window, cx);
            }))
            .child(
                self.tool_button("tool-terminal", "Terminal", cx, |this, window, cx| {
                    this.toggle_terminal_panel(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-search", "Search", cx, |this, window, cx| {
                    this.open_project_search(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-tasks", "Tasks", cx, |this, window, cx| {
                    this.open_tasks_modal(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-problems", "Problems", cx, |this, window, cx| {
                    this.open_diagnostics(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-file-finder", "Files...", cx, |this, window, cx| {
                    this.open_file_finder(window, cx);
                }),
            )
            .child(
                self.tool_button("tool-commands", "Commands", cx, |this, window, cx| {
                    this.open_command_palette(window, cx);
                }),
            )
            .when(self.feature_policy.show_ai, |el| {
                el.child(
                    self.tool_button("tool-agent", agent_label, cx, |this, window, cx| {
                        this.toggle_agent_panel(window, cx);
                    }),
                )
            })
    }
}

impl Focusable for MobileWorkspaceChrome {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.workspace.read(cx).focus_handle(cx)
    }
}

impl Render for MobileWorkspaceChrome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let modal_layer = self.workspace.read(cx).modal_layer();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x020617))
            .child(self.render_header(cx))
            .when(self.port_forward_manager.read(cx).has_forwards(), |el| {
                el.child(self.render_port_forward_status(cx))
            })
            .child(self.render_tool_strip(cx))
            .child(div().flex_1().min_h(px(0.0)).child(self.workspace.clone()))
            .child(modal_layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_status_labels_match_minimal_wrapper_copy() {
        assert_eq!(
            MobileWorkspaceChrome::recovery_status_label(SessionRecoveryState::Healthy),
            "Connected"
        );
        assert_eq!(
            MobileWorkspaceChrome::recovery_status_label(SessionRecoveryState::Reconnecting),
            "Reconnecting"
        );
        assert_eq!(
            MobileWorkspaceChrome::recovery_status_label(
                SessionRecoveryState::ReconnectFailedRetryable
            ),
            "Reconnect failed"
        );
        assert_eq!(
            MobileWorkspaceChrome::recovery_status_label(SessionRecoveryState::ReconnectExhausted),
            "Disconnected"
        );
    }
}
