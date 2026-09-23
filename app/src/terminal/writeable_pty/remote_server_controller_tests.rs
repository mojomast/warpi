use super::{
    connection_label_from_session_hosts, connection_label_from_ssh_host,
    connection_label_from_user_and_host, effective_ssh_extension_install_mode,
};
use crate::terminal::warpify::settings::SshExtensionInstallMode;

#[test]
fn connection_label_prefers_ssh_host_over_reported_hostname() {
    assert_eq!(
        connection_label_from_session_hosts(
            "moira",
            "remote-reported-hostname",
            Some("ssh-user@devbox.namespace"),
        ),
        "moira@devbox.namespace"
    );
    assert_eq!(
        connection_label_from_session_hosts("moira", "remote-reported-hostname", None),
        "moira@remote-reported-hostname"
    );
}

#[test]
fn connection_label_from_ssh_host_strips_user_prefix() {
    assert_eq!(
        connection_label_from_ssh_host("moira@moira.devbox.namespace"),
        "moira.devbox.namespace"
    );
    assert_eq!(
        connection_label_from_ssh_host("moira.devbox.namespace"),
        "moira.devbox.namespace"
    );
}

#[test]
fn connection_label_from_user_and_host_matches_udi_format() {
    assert_eq!(
        connection_label_from_user_and_host("kevinyang", Some("ssh-testing")),
        "kevinyang@ssh-testing"
    );
    assert_eq!(
        connection_label_from_user_and_host("kevinyang", None),
        "kevinyang"
    );
    assert_eq!(
        connection_label_from_user_and_host("", Some("ssh-testing")),
        "ssh-testing"
    );
    assert_eq!(connection_label_from_user_and_host("", None), "Remote host");
}

#[test]
fn standalone_never_installs_the_remote_server() {
    for configured in [
        SshExtensionInstallMode::AlwaysAsk,
        SshExtensionInstallMode::AlwaysInstall,
        SshExtensionInstallMode::NeverInstall,
    ] {
        assert_eq!(
            effective_ssh_extension_install_mode(configured, true),
            SshExtensionInstallMode::NeverInstall,
            "{configured:?} must collapse to NeverInstall in standalone mode"
        );
    }
}

#[test]
fn configured_install_mode_is_unchanged_when_standalone_is_off() {
    for configured in [
        SshExtensionInstallMode::AlwaysAsk,
        SshExtensionInstallMode::AlwaysInstall,
        SshExtensionInstallMode::NeverInstall,
    ] {
        assert_eq!(
            effective_ssh_extension_install_mode(configured, false),
            configured
        );
    }
}
