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
fn ultra_skill_installs_without_overwriting_regular_or_easy_skills() {
    let hosts = [".codex", ".claude", ".kimi-code"];
    for target in ["codex", "all"] {
        let home = tempfile::tempdir().unwrap();
        for flags in [
            vec!["install-skill", "--host", "all", "--yes"],
            vec!["install-skill", "--host", "all", "--easy-mode", "--yes"],
        ] {
            Command::cargo_bin("triad")
                .unwrap()
                .env("HOME", home.path())
                .args(flags)
                .assert()
                .success();
        }
        let mut originals = Vec::new();
        for host in hosts {
            for skill in ["triad", "triad-easy"] {
                let path = home.path().join(host).join("skills").join(skill);
                let instruction = path.join("SKILL.md");
                originals.push((instruction.clone(), std::fs::read(instruction).unwrap()));
                if host == ".codex" {
                    let metadata = path.join("agents/openai.yaml");
                    originals.push((metadata.clone(), std::fs::read(metadata).unwrap()));
                }
            }
        }

        Command::cargo_bin("triad")
            .unwrap()
            .env("HOME", home.path())
            .args(["install-skill", "--host", target, "--ultra-mode"])
            .assert()
            .code(2)
            .stdout(predicate::str::contains("No changes made"));
        for host in hosts {
            assert!(!home.path().join(host).join("skills/triad-ultra").exists());
        }
        for (path, before) in &originals {
            assert_eq!(&std::fs::read(path).unwrap(), before);
        }

        Command::cargo_bin("triad")
            .unwrap()
            .env("HOME", home.path())
            .args(["install-skill", "--host", target, "--ultra-mode", "--yes"])
            .assert()
            .success();
        let mut ultra_skills = Vec::new();
        for host in hosts {
            let path = home.path().join(host).join("skills/triad-ultra/SKILL.md");
            if target == "codex" && host != ".codex" {
                assert!(!path.exists());
                continue;
            }
            let ultra = std::fs::read_to_string(path).unwrap();
            for expected in [
                "name: triad-ultra",
                "--ultra-mode",
                "claude-opus-5-5",
                "gpt-6-astra",
                "Ultracode",
                "reasoning `ultra`",
                "Fast",
                "Ultrafast",
                "take the run ID, not `--ultra-mode`",
                "separate explicit user approval",
                "No edits/deletes",
            ] {
                assert!(
                    ultra.contains(expected),
                    "missing {expected} for {target}/{host}"
                );
            }
            ultra_skills.push(ultra);
        }
        assert!(ultra_skills.windows(2).all(|pair| pair[0] == pair[1]));
        for (path, before) in &originals {
            assert_eq!(&std::fs::read(path).unwrap(), before);
        }
        let metadata = std::fs::read_to_string(
            home.path()
                .join(".codex/skills/triad-ultra/agents/openai.yaml"),
        )
        .unwrap();
        assert!(metadata.contains("display_name: \"Triad Ultra\""));
        assert!(metadata.contains("$triad-ultra"));
        assert!(metadata.contains("allow_implicit_invocation: true"));
    }
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
    assert!(!args.ultra_mode);
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
    assert!(!args.ultra_mode);
    for cmd in ["review", "providers", "doctor"] {
        Command::cargo_bin("triad")
            .unwrap()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--easy-mode"));
    }
}

#[test]
fn ultra_mode_is_explicit_persistent_and_exclusive_with_easy_mode() {
    use clap::Parser;
    use triad::cli::{Cli, Command as CliCommand, ReviewArgs};

    let CliCommand::Review(args) =
        Cli::parse_from(["triad", "review", "--ultra-mode", "--detach"]).command
    else {
        panic!("expected review");
    };
    assert!(args.ultra_mode);
    assert!(!args.easy_mode);
    let mut request = serde_json::to_value(args).unwrap();
    let decoded: ReviewArgs = serde_json::from_value(request.clone()).unwrap();
    assert!(decoded.ultra_mode);
    assert!(!decoded.easy_mode);
    request.as_object_mut().unwrap().remove("ultra_mode");
    assert!(
        !serde_json::from_value::<ReviewArgs>(request)
            .unwrap()
            .ultra_mode
    );

    for cmd in ["review", "providers", "doctor", "install-skill"] {
        let mut base = vec!["triad", cmd];
        if cmd == "install-skill" {
            base.extend(["--host", "codex"]);
        }
        let mut ultra_args = base.clone();
        ultra_args.push("--ultra-mode");
        assert!(Cli::try_parse_from(ultra_args).is_ok());
        for flags in [
            ["--easy-mode", "--ultra-mode"],
            ["--ultra-mode", "--easy-mode"],
        ] {
            let mut conflicting_args = base.clone();
            conflicting_args.extend(flags);
            let error = Cli::try_parse_from(conflicting_args).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
        Command::cargo_bin("triad")
            .unwrap()
            .args([cmd, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--ultra-mode"));
    }
}
