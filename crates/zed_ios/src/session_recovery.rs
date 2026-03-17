use gpui::{App, AppContext, Context, Entity, Global, IosLifecycleEvent, Subscription};
use remote::{RecoveryState, RemoteClient};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionLifecyclePhase {
    Active,
    Inactive,
    Background,
    ForegroundPending,
    Terminating,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRecoveryState {
    Healthy,
    Reconnecting,
    ReconnectFailedRetryable,
    ReconnectExhausted,
}

struct GlobalSessionRecoveryCoordinator(Entity<SessionRecoveryCoordinator>);

impl Global for GlobalSessionRecoveryCoordinator {}

pub struct SessionRecoveryCoordinator {
    remote_client: Option<Entity<RemoteClient>>,
    connection_profile_id: Option<i64>,
    last_remote_path: Option<String>,
    lifecycle_phase: SessionLifecyclePhase,
    recovery_state: SessionRecoveryState,
    remote_client_subscription: Option<Subscription>,
}

impl SessionRecoveryCoordinator {
    pub fn init(cx: &mut App) {
        let coordinator = cx.new(|_| Self::new());
        cx.set_global(GlobalSessionRecoveryCoordinator(coordinator));
    }

    fn new() -> Self {
        Self {
            remote_client: None,
            connection_profile_id: None,
            last_remote_path: None,
            lifecycle_phase: SessionLifecyclePhase::Active,
            recovery_state: SessionRecoveryState::Healthy,
            remote_client_subscription: None,
        }
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSessionRecoveryCoordinator>()
            .map(|global| global.0.clone())
    }

    pub fn handle_ios_lifecycle_event(event: IosLifecycleEvent, cx: &mut App) {
        let Some(coordinator) = Self::global(cx) else {
            return;
        };
        let _ = coordinator.update(cx, |coordinator, cx| {
            coordinator.handle_lifecycle_event(event, cx);
        });
    }

    pub fn install_session(
        remote_client: Entity<RemoteClient>,
        connection_profile_id: Option<i64>,
        last_remote_path: Option<String>,
        cx: &mut App,
    ) {
        let Some(coordinator) = Self::global(cx) else {
            return;
        };
        let _ = coordinator.update(cx, |coordinator, cx| {
            coordinator.set_session(remote_client, connection_profile_id, last_remote_path, cx);
        });
    }

    pub fn clear_session(cx: &mut App) {
        let Some(coordinator) = Self::global(cx) else {
            return;
        };
        let _ = coordinator.update(cx, |coordinator, cx| coordinator.clear(cx));
    }

    pub fn state(&self) -> SessionRecoveryState {
        self.recovery_state
    }

    pub fn lifecycle_phase(&self) -> SessionLifecyclePhase {
        self.lifecycle_phase
    }

    fn set_session(
        &mut self,
        remote_client: Entity<RemoteClient>,
        connection_profile_id: Option<i64>,
        last_remote_path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.remote_client_subscription.take();
        self.remote_client = Some(remote_client.clone());
        self.connection_profile_id = connection_profile_id;
        self.last_remote_path = last_remote_path;
        self.remote_client_subscription = Some(cx.observe(&remote_client, |this, _, cx| {
            this.sync_state(cx);
        }));
        self.sync_state(cx);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.remote_client_subscription.take();
        self.remote_client = None;
        self.connection_profile_id = None;
        self.last_remote_path = None;
        self.set_recovery_state(SessionRecoveryState::Healthy);
        cx.notify();
    }

    fn handle_lifecycle_event(&mut self, event: IosLifecycleEvent, cx: &mut Context<Self>) {
        self.lifecycle_phase = match event {
            IosLifecycleEvent::DidBecomeActive => SessionLifecyclePhase::Active,
            IosLifecycleEvent::WillResignActive => SessionLifecyclePhase::Inactive,
            IosLifecycleEvent::DidEnterBackground => SessionLifecyclePhase::Background,
            IosLifecycleEvent::WillEnterForeground => SessionLifecyclePhase::ForegroundPending,
            IosLifecycleEvent::WillTerminate => SessionLifecyclePhase::Terminating,
        };
        self.sync_state(cx);
    }

    fn sync_state(&mut self, cx: &mut Context<Self>) {
        let next_state = self
            .remote_client
            .as_ref()
            .and_then(|client| client.read_with(cx, |client, _| Some(client.recovery_state())))
            .map(Self::map_recovery_state)
            .unwrap_or(SessionRecoveryState::Healthy);
        self.set_recovery_state(next_state);
        cx.notify();
    }

    fn map_recovery_state(state: RecoveryState) -> SessionRecoveryState {
        match state {
            RecoveryState::Connected => SessionRecoveryState::Healthy,
            RecoveryState::Connecting
            | RecoveryState::HeartbeatMissed
            | RecoveryState::Reconnecting => SessionRecoveryState::Reconnecting,
            RecoveryState::ReconnectFailed => SessionRecoveryState::ReconnectFailedRetryable,
            RecoveryState::ReconnectExhausted
            | RecoveryState::ServerNotRunning
            | RecoveryState::Disconnected => SessionRecoveryState::ReconnectExhausted,
        }
    }

    fn set_recovery_state(&mut self, next_state: SessionRecoveryState) {
        if self.recovery_state == next_state {
            return;
        }

        self.recovery_state = next_state;
        match next_state {
            SessionRecoveryState::Healthy => telemetry::event!("iOS Recovery Succeeded"),
            SessionRecoveryState::Reconnecting => telemetry::event!("iOS Recovery Started"),
            SessionRecoveryState::ReconnectFailedRetryable => {
                telemetry::event!("iOS Recovery Failed")
            }
            SessionRecoveryState::ReconnectExhausted => {
                telemetry::event!("iOS Recovery Exhausted")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_state_mapping_matches_mobile_expectations() {
        assert_eq!(
            SessionRecoveryCoordinator::map_recovery_state(RecoveryState::Connected),
            SessionRecoveryState::Healthy
        );
        assert_eq!(
            SessionRecoveryCoordinator::map_recovery_state(RecoveryState::Reconnecting),
            SessionRecoveryState::Reconnecting
        );
        assert_eq!(
            SessionRecoveryCoordinator::map_recovery_state(RecoveryState::ReconnectFailed),
            SessionRecoveryState::ReconnectFailedRetryable
        );
        assert_eq!(
            SessionRecoveryCoordinator::map_recovery_state(RecoveryState::ReconnectExhausted),
            SessionRecoveryState::ReconnectExhausted
        );
    }
}
