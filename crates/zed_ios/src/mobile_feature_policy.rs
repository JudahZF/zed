use gpui::{App, Pixels};
use settings::Settings;

use crate::mobile_settings::MobileSettings;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct MobileFeaturePolicy {
    pub restore_last_session: bool,
    pub use_shared_remote_restore: bool,
    pub show_ai: bool,
    pub mobile_agent_review: bool,
    pub compact_panels: bool,
    pub mobile_git_modals: bool,
    pub terminal_default_height: Pixels,
}

impl MobileFeaturePolicy {
    pub fn from_app(cx: &App) -> Self {
        let settings = MobileSettings::get_global(cx);
        Self {
            restore_last_session: settings.restore_last_session,
            use_shared_remote_restore: settings.use_shared_remote_restore,
            show_ai: settings.show_ai,
            mobile_agent_review: settings.mobile_agent_review,
            compact_panels: settings.compact_panels,
            mobile_git_modals: settings.mobile_git_modals,
            terminal_default_height: settings.terminal_default_height,
        }
    }
}
