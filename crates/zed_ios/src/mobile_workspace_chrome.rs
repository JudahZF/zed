use std::sync::Arc;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, div, px, rgb,
};
use outline_panel::OutlinePanel;
use project_panel::ProjectPanel;
use remote::RemoteClient;
use ui::prelude::*;
use workspace::dock::PanelHandle;
use workspace::{AppState, Workspace};

use crate::persistence::{ConnectionDb, ConnectionProfile};
use crate::port_forward_manager::PortForwardManager;
use crate::root_view::RootView;
use crate::session_recovery::{
    SessionLifecyclePhase, SessionRecoveryCoordinator, SessionRecoveryState,
};

pub struct MobileWorkspaceChrome {
    workspace: Entity<Workspace>,
    app_state: Arc<AppState>,
    recovery_coordinator: Entity<SessionRecoveryCoordinator>,
    port_forward_manager: Entity<PortForwardManager>,
    active_connection_profile: Option<ConnectionProfile>,
}

impl MobileWorkspaceChrome {
    pub fn new(
        workspace: Entity<Workspace>,
        remote_client: Entity<RemoteClient>,
        connection_profile_id: Option<i64>,
        remote_path: Option<String>,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_connection_profile = connection_profile_id.and_then(|id| {
            ConnectionDb::open()
                .ok()?
                .connection_profile_by_id(id)
                .ok()?
        });
        let desired_forwards = active_connection_profile
            .as_ref()
            .map(|profile| profile.port_forwards.clone())
            .unwrap_or_default();
        let recovery_coordinator = SessionRecoveryCoordinator::global(cx)
            .expect("SessionRecoveryCoordinator must be initialized before opening iOS workspace");
        SessionRecoveryCoordinator::install_session(
            remote_client.clone(),
            connection_profile_id,
            remote_path,
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
            recovery_coordinator,
            port_forward_manager,
            active_connection_profile,
        };

        cx.observe(&this.recovery_coordinator, |this, _, cx| {
            this.sync_runtime_state(cx);
            cx.notify();
        })
        .detach();

        this.sync_runtime_state(cx);
        this.load_panels(window, cx);
        this
    }

    fn load_panels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace_handle = self.workspace.downgrade();
        let sidebar_width = px(240.0);

        window
            .spawn(cx, async move |cx| {
                match ProjectPanel::load(workspace_handle.clone(), cx.clone()).await {
                    Ok(panel) => {
                        if let Err(err) = workspace_handle.update_in(cx, |workspace, window, cx| {
                            panel.set_size(Some(sidebar_width), window, cx);
                            workspace.add_panel(panel, window, cx);
                            workspace.open_panel::<ProjectPanel>(window, cx);
                        }) {
                            log::error!("[Zed iOS] Failed to attach project panel: {err}");
                        }
                    }
                    Err(err) => log::error!("[Zed iOS] Failed to load project panel: {err:#}"),
                }

                match OutlinePanel::load(workspace_handle.clone(), cx.clone()).await {
                    Ok(panel) => {
                        if let Err(err) = workspace_handle.update_in(cx, |workspace, window, cx| {
                            panel.set_size(Some(sidebar_width), window, cx);
                            workspace.add_panel(panel, window, cx);
                        }) {
                            log::error!("[Zed iOS] Failed to attach outline panel: {err}");
                        }
                    }
                    Err(err) => log::error!("[Zed iOS] Failed to load outline panel: {err:#}"),
                }
            })
            .detach();
    }

    fn sync_runtime_state(&mut self, cx: &mut Context<Self>) {
        let lifecycle_phase = self.recovery_coordinator.read(cx).lifecycle_phase();
        let recovery_state = self.recovery_coordinator.read(cx).state();
        let should_suspend_forwards = !matches!(lifecycle_phase, SessionLifecyclePhase::Active)
            || recovery_state != SessionRecoveryState::Healthy;

        self.port_forward_manager.update(cx, |manager, cx| {
            manager.set_suspended(should_suspend_forwards, cx);
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
}

impl Focusable for MobileWorkspaceChrome {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.workspace.read(cx).focus_handle(cx)
    }
}

impl Render for MobileWorkspaceChrome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x020617))
            .child(self.render_header(cx))
            .child(div().flex_1().min_h(px(0.0)).child(self.workspace.clone()))
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
