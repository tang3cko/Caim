mod common;
use common::*;
use std::fs;
use std::path::Path;

fn init(dir: &Path, args: &[&str]) -> (i32, String) {
    run(caim().arg("--init").args(args).current_dir(dir).env_remove("CAIM_ROOT"))
}

fn in_project(dir: &Path, args: &[&str]) -> (i32, String) {
    run(caim().args(args).current_dir(dir).env_remove("CAIM_ROOT").env("CAIM_LOCK_DIR", dir.join(".locks")))
}

#[test]
fn init_writes_a_project_that_runs() {
    let temp = TempDir::new();
    let dir = temp.path("My Game");
    fs::create_dir(&dir).unwrap();

    let (status, output) = init(&dir, &[]);
    assert_eq!(status, 0, "{output}");
    let written = fs::read_to_string(dir.join("caim.toml")).unwrap();
    assert!(written.contains("name = \"my-game\""), "{written}");

    let (status, output) = in_project(&dir, &["list"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("hello"), "{output}");
    let (status, output) = in_project(&dir, &["hello"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("[hello] exit=0"), "{output}");
}

#[test]
fn the_commented_reference_in_an_initialized_file_is_valid() {
    let temp = TempDir::new();
    let dir = temp.path("game");
    fs::create_dir(&dir).unwrap();
    assert_eq!(init(&dir, &[]).0, 0);

    // Lines of the reference are commented with "# "; prose is commented with "##".
    let written = fs::read_to_string(dir.join("caim.toml")).unwrap();
    let uncommented: String = written
        .lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(rest) => format!("{rest}\n"),
            None => format!("{line}\n"),
        })
        .collect();
    assert_ne!(uncommented, written);
    fs::write(dir.join("caim.toml"), &uncommented).unwrap();

    let (status, output) = in_project(&dir, &["list"]);
    assert_eq!(status, 0, "{output}\n{uncommented}");
    let (status, output) = in_project(&dir, &["--dry-run", "build", "--profile", "release"]);
    assert_eq!(status, 0, "{output}\n{uncommented}");
}

#[test]
fn init_leaves_an_existing_caim_toml_alone() {
    let temp = TempDir::new();
    fs::write(temp.path("caim.toml"), "name = \"mine\"\n").unwrap();

    let (status, output) = init(&temp.0, &[]);
    assert_eq!(status, 73, "{output}");
    assert!(output.contains("already"), "{output}");
    assert_eq!(fs::read_to_string(temp.path("caim.toml")).unwrap(), "name = \"mine\"\n");
}

#[test]
fn init_names_a_project_whose_directory_name_cannot_be_one() {
    let temp = TempDir::new();
    let dir = temp.path("2024");
    fs::create_dir(&dir).unwrap();

    assert_eq!(init(&dir, &[]).0, 0);
    let written = fs::read_to_string(dir.join("caim.toml")).unwrap();
    assert!(written.contains("name = \"project\""), "{written}");
    assert_eq!(in_project(&dir, &["list"]).0, 0);
}

#[test]
fn init_takes_no_arguments() {
    let temp = TempDir::new();
    let (status, _) = init(&temp.0, &["extra"]);
    assert_eq!(status, 64);
    assert!(!temp.path("caim.toml").exists());
}
