//! End-to-end checks for the command-line surface that do not need network.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn isolated() -> (Command, TempDir, TempDir) {
    let home = TempDir::new().expect("home tempdir");
    let project = TempDir::new().expect("project tempdir");
    let mut command = Command::cargo_bin("akc").expect("akc binary");
    command
        .env("AAYKRA_HOME", home.path())
        .current_dir(project.path());
    (command, home, project)
}

#[test]
fn version_and_help_are_available() {
    Command::cargo_bin("akc")
        .expect("akc binary")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("aaykra-code"));

    Command::cargo_bin("akc")
        .expect("akc binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("serve")
                .and(predicate::str::contains("sessions"))
                .and(predicate::str::contains("--permission-mode")),
        );
}

#[test]
fn config_template_contains_key_sections() {
    Command::cargo_bin("akc")
        .expect("akc binary")
        .args(["config", "template"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("[permissions]")
                .and(predicate::str::contains("[sandbox]"))
                .and(predicate::str::contains("[context]"))
                .and(predicate::str::contains("providers.")),
        );
}

#[test]
fn init_scaffolds_config_and_directories() {
    let (mut command, _home, project) = isolated();
    command
        .args(["init", "--no-instructions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"));

    assert!(project.path().join(".aaykra/config.toml").is_file());
    assert!(project.path().join(".aaykra/agents").is_dir());
    assert!(project.path().join(".aaykra/skills").is_dir());
    assert!(!project.path().join("AAYKRA.md").exists());
}

#[test]
fn models_json_lists_builtin_providers() {
    let (mut command, _home, _project) = isolated();
    let output = command
        .args(["--provider", "anthropic", "models", "--json"])
        .output()
        .expect("run models");
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).expect("models json");
    let providers = value.as_object().expect("models should be a provider map");
    assert!(providers.contains_key("anthropic"), "{value}");
    assert!(value["anthropic"]["models"].is_array(), "{value}");
    assert_eq!(value["anthropic"]["protocol"], "anthropic");
}

#[test]
fn doctor_json_reports_a_healthy_configuration() {
    let (mut command, home, _project) = isolated();
    let output = command
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor");
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).expect("doctor json");
    assert_eq!(value["config_ok"], true, "{value}");
    assert_eq!(
        value["aaykra_home"].as_str(),
        Some(home.path().to_str().expect("utf-8 home path"))
    );
    assert!(value["providers"].is_array());
}
