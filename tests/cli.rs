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

#[test]
fn easy_skill_installs_alongside_regular_skill_for_all_hosts() {
    let home = tempfile::tempdir().unwrap();
    Command::cargo_bin("triad")
        .unwrap()
        .env("HOME", home.path())
        .args(["install-skill", "--host", "all", "--yes"])
        .assert()
        .success();
    let hosts = [".codex", ".claude", ".kimi-code"];
    let regular: Vec<_> = hosts
        .iter()
        .map(|host| std::fs::read(home.path().join(host).join("skills/triad/SKILL.md")).unwrap())
        .collect();
    let original_metadata =
        std::fs::read(home.path().join(".codex/skills/triad/agents/openai.yaml")).unwrap();

    Command::cargo_bin("triad")
        .unwrap()
        .env("HOME", home.path())
        .args(["install-skill", "--host", "all", "--easy-mode"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("No changes made"));
    for host in hosts {
        assert!(!home.path().join(host).join("skills/triad-easy").exists());
    }
    Command::cargo_bin("triad")
        .unwrap()
        .env("HOME", home.path())
        .args(["install-skill", "--host", "all", "--easy-mode", "--yes"])
        .assert()
        .success();
    let mut easy_skills = Vec::new();
    for (host, before) in hosts.iter().zip(regular) {
        assert_eq!(
            std::fs::read(home.path().join(host).join("skills/triad/SKILL.md")).unwrap(),
            before
        );
        let easy =
            std::fs::read_to_string(home.path().join(host).join("skills/triad-easy/SKILL.md"))
                .unwrap();
        for expected in [
            "name: triad-easy",
            "--easy-mode",
            "claude-opus-5-5",
            "gpt-6.1-sol",
            "separate explicit user approval",
            "No edits/deletes",
        ] {
            assert!(easy.contains(expected), "missing {expected}");
        }
        easy_skills.push(easy);
    }
    assert!(easy_skills.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        std::fs::read(home.path().join(".codex/skills/triad/agents/openai.yaml")).unwrap(),
        original_metadata
    );
    let metadata = std::fs::read_to_string(
        home.path()
            .join(".codex/skills/triad-easy/agents/openai.yaml"),
    )
    .unwrap();
    assert!(metadata.contains("$triad-easy"));
    assert!(metadata.contains("allow_implicit_invocation: true"));
}

#[test]
fn easy_mode_is_opt_in_and_old_requests_still_deserialize() {
    use clap::Parser;
    use triad::cli::{Cli, Command as CliCommand, ReviewArgs};

    let CliCommand::Review(args) =
        Cli::parse_from(["triad", "review", "--easy-mode", "--detach"]).command
    else {
        panic!("expected review");
    };
    assert!(args.easy_mode);
    let mut request = serde_json::to_value(&args).unwrap();
    assert!(
        serde_json::from_value::<ReviewArgs>(request.clone())
            .unwrap()
            .easy_mode
    );
    request.as_object_mut().unwrap().remove("easy_mode");
    assert!(
        !serde_json::from_value::<ReviewArgs>(request)
            .unwrap()
            .easy_mode
    );
    let CliCommand::Review(args) = Cli::parse_from(["triad", "review"]).command else {
        panic!("expected review");
    };
    assert!(!args.easy_mode);
    for cmd in ["review", "providers", "doctor"] {
        Command::cargo_bin("triad")
            .unwrap()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--easy-mode"));
    }
}
