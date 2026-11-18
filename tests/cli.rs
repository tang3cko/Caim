mod common;
use caim::commands::{BUILTIN, find, reserved};
use common::*;
use std::collections::BTreeSet;
use std::fs;
use std::process::{Command, Output, Stdio};

fn outside(dir: &TempDir, args: &[&str]) -> (i32, String, String) {
    let mut command = caim();
    for (key, _) in std::env::vars() {
        if key.starts_with("CAIM_") {
            command.env_remove(key);
        }
    }
    split(command.args(args).current_dir(&dir.0))
}

fn split(command: &mut Command) -> (i32, String, String) {
    let Output { status, stdout, stderr } = command.stdin(Stdio::null()).output().unwrap();
    (status_of(status), String::from_utf8_lossy(&stdout).into(), String::from_utf8_lossy(&stderr).into())
}

#[test]
fn every_word_runs_one_command_and_no_task_can_take_it() {
    let mut seen = BTreeSet::new();
    for command in BUILTIN {
        assert!(command.usage.starts_with(&format!("usage: caim {}", command.name)), "{}", command.name);
        for word in std::iter::once(&command.name).chain(command.aliases) {
            assert!(seen.insert(*word), "{word} runs two commands");
            assert_eq!(find(word).map(|found| found.name), Some(command.name), "{word}");
            assert!(reserved(word), "{word}");
        }
    }
}

#[test]
fn every_command_explains_itself_without_a_project() {
    let dir = TempDir::new();
    for command in BUILTIN {
        for words in [vec![command.name, "--help"], vec![command.name, "-h"], vec!["help", command.name]] {
            let (status, stdout, stderr) = outside(&dir, &words);
            assert_eq!((status, stdout.as_str()), (0, command.usage), "{words:?}: {stderr}");
        }
    }
    assert!(!dir.path("caim.toml").exists(), "a request for help wrote a caim.toml");
}

#[test]
fn general_help_names_every_command() {
    let dir = TempDir::new();
    for help in [vec![], vec!["help"], vec!["--help"], vec!["-h"]] {
        let (status, stdout, stderr) = outside(&dir, &help);
        assert_eq!(status, 0, "{help:?}: {stdout}{stderr}");
        for command in BUILTIN {
            assert!(stdout.contains(&format!("\n  {:<14} {}\n", command.name, command.about)), "{help:?}: {stdout}");
        }
    }
}

#[test]
fn the_readme_documents_every_command() {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    for command in BUILTIN {
        assert!(readme.contains(&format!("caim {}", command.name)), "README.md does not show caim {}", command.name);
    }
}

#[test]
fn version_needs_no_project() {
    let dir = TempDir::new();
    let expected = format!("caim {}\n", env!("CARGO_PKG_VERSION"));
    for word in ["version", "--version", "-V"] {
        let (status, stdout, stderr) = outside(&dir, &[word]);
        assert_eq!((status, stdout.as_str(), stderr.as_str()), (0, expected.as_str(), ""), "{word}");
    }
    let (status, _, stderr) = outside(&dir, &["version", "extra"]);
    assert_eq!(status, 64, "{stderr}");

    fs::write(dir.path("caim.toml"), "name = \"x\"\n").unwrap();
    let (status, stdout, stderr) = outside(&dir, &["version"]);
    assert_eq!((status, stdout.as_str()), (0, expected.as_str()), "inside a project: {stderr}");
}

#[test]
fn init_needs_no_project_and_answers_to_its_old_flag() {
    for word in ["init", "--init"] {
        let dir = TempDir::new();
        let (status, _, stderr) = outside(&dir, &[word]);
        assert_eq!(status, 0, "{word}: {stderr}");
        assert!(dir.path("caim.toml").is_file(), "{word}");
    }
}
