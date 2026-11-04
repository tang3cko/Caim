mod common;
use common::*;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn shasum(dir: &TempDir, names: &[&str]) -> String {
    let output = Command::new("shasum").arg("-a").arg("256").args(names).current_dir(&dir.0).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn hash(dir: &TempDir, input: &[u8]) -> (i32, String) {
    let mut child =
        caim().arg("hash").current_dir(&dir.0).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    (output.status.code().unwrap(), String::from_utf8(output.stdout).unwrap())
}

fn check(dir: &TempDir, lists: &[&str]) -> (i32, String) {
    let output = caim().arg("hash-check").args(lists).current_dir(&dir.0).output().unwrap();
    (output.status.code().unwrap(), String::from_utf8(output.stdout).unwrap())
}

#[test]
fn hash_matches_shasum() {
    let dir = TempDir::new();
    fs::write(dir.path("a"), "first\n").unwrap();
    fs::write(dir.path("b c"), vec![7u8; 3 << 20]).unwrap();
    fs::write(dir.path("empty"), "").unwrap();
    let (status, output) = hash(&dir, b"a\0b c\0empty\0");
    assert_eq!(status, 0);
    assert_eq!(output, shasum(&dir, &["a", "b c", "empty"]));
    assert_eq!(hash(&dir, b"a\0missing\0"), (1, String::new()));
}

#[test]
fn hash_check_names_changed_and_missing_files_once_per_line() {
    let dir = TempDir::new();
    for name in ["kept", "edited", "gone"] {
        fs::write(dir.path(name), name).unwrap();
    }
    fs::write(dir.path("one.reads"), shasum(&dir, &["kept", "edited"])).unwrap();
    fs::write(dir.path("two.reads"), shasum(&dir, &["edited", "gone", "kept"])).unwrap();
    assert_eq!(check(&dir, &["one.reads", "two.reads"]), (0, String::new()));

    fs::write(dir.path("edited"), "changed").unwrap();
    fs::remove_file(dir.path("gone")).unwrap();
    assert_eq!(
        check(&dir, &["one.reads", "two.reads"]),
        (1, "one.reads\tedited\ntwo.reads\tedited\ntwo.reads\tgone\n".to_string())
    );
}

#[test]
fn hash_check_refuses_what_it_cannot_read() {
    let dir = TempDir::new();
    fs::write(dir.path("bad.reads"), "not a digest  file\n").unwrap();
    assert_eq!(check(&dir, &["bad.reads"]).0, 64);
    assert_eq!(check(&dir, &[]).0, 64);
    assert_eq!(check(&dir, &["absent.reads"]).0, 2);
}
