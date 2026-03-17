use gpui::{Pixels, px};
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Clone, Copy, Debug, RegisterSetting)]
pub struct MobileSettings {
    pub restore_last_session: bool,
    pub use_shared_remote_restore: bool,
    pub show_ai: bool,
    pub mobile_agent_review: bool,
    pub compact_panels: bool,
    pub mobile_git_modals: bool,
    pub terminal_default_height: Pixels,
}

impl Settings for MobileSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let mobile = content.mobile.clone().unwrap_or_default();
        Self {
            restore_last_session: mobile.restore_last_session.unwrap_or(true),
            use_shared_remote_restore: mobile.use_shared_remote_restore.unwrap_or(true),
            show_ai: mobile.show_ai.unwrap_or(true),
            mobile_agent_review: mobile.mobile_agent_review.unwrap_or(true),
            compact_panels: mobile.compact_panels.unwrap_or(true),
            mobile_git_modals: mobile.mobile_git_modals.unwrap_or(true),
            terminal_default_height: px(mobile.terminal_default_height.unwrap_or(260.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::MobileSettingsContent;

    #[test]
    fn mobile_settings_default_when_namespace_is_absent() {
        let settings = MobileSettings::from_settings(&SettingsContent::default());
        assert!(settings.restore_last_session);
        assert!(settings.use_shared_remote_restore);
        assert!(settings.show_ai);
        assert!(settings.mobile_agent_review);
        assert!(settings.compact_panels);
        assert!(settings.mobile_git_modals);
        assert_eq!(settings.terminal_default_height, px(260.0));
    }

    #[test]
    fn mobile_settings_honor_overrides() {
        let mut content = SettingsContent::default();
        content.mobile = Some(MobileSettingsContent {
            restore_last_session: Some(false),
            use_shared_remote_restore: Some(false),
            show_ai: Some(false),
            mobile_agent_review: Some(false),
            compact_panels: Some(false),
            mobile_git_modals: Some(false),
            terminal_default_height: Some(320.0),
        });

        let settings = MobileSettings::from_settings(&content);
        assert!(!settings.restore_last_session);
        assert!(!settings.use_shared_remote_restore);
        assert!(!settings.show_ai);
        assert!(!settings.mobile_agent_review);
        assert!(!settings.compact_panels);
        assert!(!settings.mobile_git_modals);
        assert_eq!(settings.terminal_default_height, px(320.0));
    }
}
