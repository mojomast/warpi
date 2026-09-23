use super::*;

const HIDDEN_SECTIONS: &[SettingsSection] = &[
    SettingsSection::Account,
    SettingsSection::BillingAndUsage,
    SettingsSection::Teams,
    SettingsSection::Referrals,
    SettingsSection::WarpDrive,
    SettingsSection::SharedBlocks,
    SettingsSection::CodeIndexing,
    SettingsSection::CloudEnvironments,
    SettingsSection::WarpCloudAgentAPIKeys,
];

const VISIBLE_SECTIONS: &[SettingsSection] = &[
    SettingsSection::Appearance,
    SettingsSection::Features,
    SettingsSection::Keybindings,
    SettingsSection::Privacy,
    SettingsSection::About,
    SettingsSection::Warpify,
    SettingsSection::Scripting,
    SettingsSection::WarpAgent,
    SettingsSection::AgentProfiles,
    SettingsSection::AgentMCPServers,
    SettingsSection::Knowledge,
    SettingsSection::ThirdPartyCLIAgents,
    SettingsSection::LocalProvider,
    SettingsSection::EditorAndCodeReview,
];

#[test]
fn account_and_cloud_sections_are_the_only_hidden_ones() {
    for section in HIDDEN_SECTIONS {
        assert!(hidden_section(*section), "{section:?} should be hidden");
    }
    for section in VISIBLE_SECTIONS {
        assert!(!hidden_section(*section), "{section:?} should stay visible");
    }
}

#[test]
fn hidden_sections_resolve_to_the_local_fallback_when_standalone_is_on() {
    for section in HIDDEN_SECTIONS {
        assert_eq!(
            resolve_settings_section_when(true, *section),
            fallback_settings_section(),
        );
    }
    for section in VISIBLE_SECTIONS {
        assert_eq!(resolve_settings_section_when(true, *section), *section);
    }
}

#[test]
fn sections_are_untouched_when_standalone_is_off() {
    for section in HIDDEN_SECTIONS.iter().chain(VISIBLE_SECTIONS) {
        assert_eq!(resolve_settings_section_when(false, *section), *section);
    }
}

#[test]
fn voice_input_is_hidden_only_in_standalone_mode() {
    assert!(hides_voice_input_when(true));
    assert!(!hides_voice_input_when(false));
}

#[test]
fn ssh_warpification_is_hidden_only_in_standalone_mode() {
    assert!(hides_ssh_warpification_when(true));
    assert!(!hides_ssh_warpification_when(false));
}
