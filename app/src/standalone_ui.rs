//! Visibility policy for UI surfaces that only make sense with a Warp account
//! or the Warp cloud backend.
//!
//! This fork can serve agent requests from a local, user-configured
//! OpenAI-compatible endpoint (the "standalone" backend). In that mode there is
//! no Warp account, no billing, and no cloud agents, so the corresponding
//! surfaces would be dead weight or actively misleading. Everything hidden in
//! that mode is listed here so the hidden set stays auditable in one place;
//! native Warp behavior is untouched while standalone mode is off.

use crate::settings_view::SettingsSection;

/// Version shown in About when the build has no compile-time release tag.
/// Keep in sync with `standalone/VERSION` and the `warp` crate version.
pub const WARPI_VERSION: &str = "v0.1.0";

/// True when the standalone (local Pi) backend is serving agent requests.
pub fn hidden_ui() -> bool {
    crate::ai::standalone::is_enabled()
}

/// Settings sections that require a Warp account or the cloud backend.
fn hidden_section(section: SettingsSection) -> bool {
    matches!(
        section,
        SettingsSection::Account
            | SettingsSection::BillingAndUsage
            | SettingsSection::Teams
            | SettingsSection::Referrals
            | SettingsSection::WarpDrive
            | SettingsSection::SharedBlocks
            | SettingsSection::CodeIndexing
            | SettingsSection::CloudEnvironments
            | SettingsSection::WarpCloudAgentAPIKeys
    )
}

/// Whether `section` must be hidden from the settings sidebar and page list.
pub fn hides_settings_section(section: SettingsSection) -> bool {
    hidden_ui() && hidden_section(section)
}

/// The page shown in place of a hidden section: a purely local page that is
/// always available.
pub fn fallback_settings_section() -> SettingsSection {
    SettingsSection::Appearance
}

fn resolve_settings_section_when(standalone: bool, section: SettingsSection) -> SettingsSection {
    if standalone && hidden_section(section) {
        fallback_settings_section()
    } else {
        section
    }
}

/// Redirects a hidden section to [`fallback_settings_section`] so deeplinks
/// (command palette, `warpctl`, session restore) land on a real page instead of
/// a blank one.
pub fn resolve_settings_section(section: SettingsSection) -> SettingsSection {
    resolve_settings_section_when(hidden_ui(), section)
}

/// Whether Warp's server-backed voice transcription must be hidden. There is no
/// local transcription path, so the feature cannot work against the Pi backend.
pub fn hides_voice_input() -> bool {
    hides_voice_input_when(hidden_ui())
}

fn hides_voice_input_when(standalone: bool) -> bool {
    standalone
}

/// Whether Warp's SSH extension (remote server) surfaces must be hidden. They
/// advertise an extension that is downloaded from Warp and cannot be installed
/// here; plain SSH keeps working.
pub fn hides_ssh_warpification() -> bool {
    hides_ssh_warpification_when(hidden_ui())
}

fn hides_ssh_warpification_when(standalone: bool) -> bool {
    standalone
}

#[cfg(test)]
#[path = "standalone_ui_tests.rs"]
mod tests;
