use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_lists_lifecycle_commands() {
    let mut command = Command::cargo_bin("triad").unwrap();
    command
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("review"))
        .stdout(predicate::str::contains("providers"))
        .stdout(predicate::str::contains("fix"));
}

#[test]
fn cursor_install_requires_confirmation() {
    let mut command = Command::cargo_bin("triad").unwrap();
    command
        .args(["provider", "install", "cursor"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("No changes made"));
}
