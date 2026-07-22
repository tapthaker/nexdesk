use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn write_config(root: &Path, role: &str, edge: Option<&str>) {
    let config_dir = root.join("nexdesk");
    fs::create_dir_all(&config_dir).unwrap();
    let edge = edge
        .map(|value| format!("switch_edge = {value:?}\n"))
        .unwrap_or_default();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "hostname = \"test-host\"\nport = 4242\nrole = {role:?}\n{edge}trusted_fingerprints = []\n"
        ),
    )
    .unwrap();
}

fn isolated_command(config_home: &Path) -> Command {
    let mut command = Command::cargo_bin("nexdesk").unwrap();
    command
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", config_home)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY");
    command
}

#[test]
fn help_is_available_from_the_real_binary() {
    Command::cargo_bin("nexdesk")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cross-platform KVM sharing tool"))
        .stdout(predicate::str::contains(
            "Usage: nexdesk [OPTIONS] <COMMAND>",
        ));
}

#[test]
fn invalid_arguments_are_rejected_by_the_real_binary() {
    Command::cargo_bin("nexdesk")
        .unwrap()
        .args(["serve", "--edge", "diagonal"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value 'diagonal'"));
}

#[test]
fn invalid_configured_role_is_rejected_before_platform_access() {
    let root = tempfile::tempdir().unwrap();
    write_config(root.path(), "spaceship", Some("right"));

    isolated_command(root.path())
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid configured role"))
        .stderr(predicate::str::contains("nexdesk setup"));
}

#[test]
fn invalid_configured_edge_is_rejected_before_platform_access() {
    let root = tempfile::tempdir().unwrap();
    write_config(root.path(), "server", Some("diagonal"));

    isolated_command(root.path())
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid configured switch edge"))
        .stderr(predicate::str::contains("--edge"));
}

#[test]
fn missing_edge_reports_a_noninteractive_error() {
    let root = tempfile::tempdir().unwrap();
    write_config(root.path(), "server", None);

    isolated_command(root.path())
        .arg("serve")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "No switch edge configured and no interactive terminal",
        ));
}
