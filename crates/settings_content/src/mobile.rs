use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

use crate::serialize_optional_f32_with_two_decimal_places;

#[with_fallible_options]
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct MobileSettingsContent {
    /// Whether Zed for iPad should automatically reconnect to the last saved remote workspace.
    ///
    /// Default: true
    pub restore_last_session: Option<bool>,
    /// Whether to use the shared remote workspace restore path on iPad.
    ///
    /// Default: true
    pub use_shared_remote_restore: Option<bool>,
    /// Whether AI entry points should be visible in iPad chrome.
    ///
    /// Default: true
    pub show_ai: Option<bool>,
    /// Whether to show the dedicated mobile agent review flow on iPad.
    ///
    /// Default: true
    pub mobile_agent_review: Option<bool>,
    /// Whether to use a denser compact chrome layout for panel controls.
    ///
    /// Default: true
    pub compact_panels: Option<bool>,
    /// Whether to prefer iPad-friendly modal Git flows over desktop-style panel interactions.
    ///
    /// Default: true
    pub mobile_git_modals: Option<bool>,
    /// Preferred default height for the iPad terminal dock, in pixels.
    ///
    /// Default: 260
    #[serde(serialize_with = "serialize_optional_f32_with_two_decimal_places")]
    pub terminal_default_height: Option<f32>,
}
