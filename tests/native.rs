mod common;
use caim::native::{judging_elsewhere, PosixFiles, Queue, Sources};
use common::*;
use std::collections::BTreeSet;
use std::fs;

fn owners_of_next(queue: &Queue, sources: &Sources) -> BTreeSet<usize> {
    let events = queue.wait(Some(180.0)).unwrap();
    assert!(!events.is_empty(), "no event before the hang guard ran out");
    events.iter().filter_map(|event| sources.owner(event)).collect()
}

fn service(queue: &Queue, sources: &mut Sources, id: usize) -> bool {
    sources.service(queue, id, 1 << 20).unwrap().0
}

fn next_progress(queue: &Queue, sources: &mut Sources) -> bool {
    loop {
        for id in owners_of_next(queue, sources) {
            if service(queue, sources, id) {
                return true;
            }
        }
    }
}

fn scan(queue: &Queue, sources: &mut Sources) -> bool {
    let mut progress = false;
    for id in 0..sources.len() {
        progress |= service(queue, sources, id);
    }
    progress
}

#[test]
fn kqueue_names_the_declared_source_a_write_a_creation_and_a_replacement_belong_to() {
    let dir = TempDir::new();
    let log = dir.text("nested/run.log");
    let jobs = dir.text("jobs");
    let queue = Queue::new().unwrap();
    let mut sources = Sources::new();
    sources.log(&queue, &log).unwrap();
    sources.directory(&queue, &jobs).unwrap();
    sources.log(&queue, &log).unwrap();
    assert_eq!(sources.len(), 2);

    fs::create_dir_all(dir.path("nested")).unwrap();
    assert!(owners_of_next(&queue, &sources).contains(&0));
    assert!(!service(&queue, &mut sources, 0));
    append(&dir.path("nested/run.log"), b"first step\n");
    assert!(owners_of_next(&queue, &sources).contains(&0));
    assert!(service(&queue, &mut sources, 0));

    append(&dir.path("nested/run.log"), b"second step\n");
    assert!(owners_of_next(&queue, &sources).contains(&0));
    assert!(service(&queue, &mut sources, 0));

    append(&dir.path("nested/next.log"), b"rebuilt\n");
    fs::rename(dir.path("nested/next.log"), dir.path("nested/run.log")).unwrap();
    assert!(owners_of_next(&queue, &sources).contains(&0));
    assert!(service(&queue, &mut sources, 0));

    fs::create_dir(&jobs).unwrap();
    assert!(owners_of_next(&queue, &sources).contains(&1));
    assert!(!service(&queue, &mut sources, 1));
    append(&dir.path("jobs/job"), b"");
    assert!(owners_of_next(&queue, &sources).contains(&1));
    assert!(service(&queue, &mut sources, 1));
}

#[test]
fn declarations_written_to_the_file_add_sources_as_they_arrive() {
    let dir = TempDir::new();
    let declared = dir.path("watch");
    let early = dir.text("early.log");
    let late = dir.text("late.log");
    append(&declared, format!("log {early}\n").as_bytes());
    let queue = Queue::new().unwrap();
    let mut sources = Sources::new();
    sources.declarations(&queue, &declared.to_string_lossy()).unwrap();
    assert_eq!(sources.len(), 2);
    append(&declared, format!("log {late}\nlog {early}\n").as_bytes());
    assert!(owners_of_next(&queue, &sources).contains(&0));
    assert!(service(&queue, &mut sources, 0));
    assert_eq!(sources.len(), 3);
    assert!(!service(&queue, &mut sources, 0));
}

#[test]
fn counted_declared_events() {
    let dir = TempDir::new();
    fs::create_dir_all(dir.path("logs")).unwrap();
    fs::create_dir_all(dir.path("jobs")).unwrap();
    fs::write(dir.path("logs/run.log"), "initial\n").unwrap();
    fs::write(dir.path("jobs/baseline.o"), b"\0\x01\x02").unwrap();
    let queue = Queue::new().unwrap();
    let mut sources = Sources::new();
    sources.log(&queue, &dir.text("logs/run.log")).unwrap();
    sources.directory(&queue, &dir.text("jobs")).unwrap();
    sources.log(&queue, &dir.text("logs/later.log")).unwrap();

    append(&dir.path("logs/run.log"), b"step alpha\n");
    assert!(next_progress(&queue, &mut sources));
    fs::write(dir.path("jobs/object.o"), b"\0\x01\x02").unwrap();
    assert!(next_progress(&queue, &mut sources));
    fs::write(dir.path("logs/later.log"), "step beta\n").unwrap();
    assert!(next_progress(&queue, &mut sources));
    let mut open = fs::OpenOptions::new().append(true).open(dir.path("logs/run.log")).unwrap();
    std::io::Write::write_all(&mut open, b"step gamma\n").unwrap();
    assert!(scan(&queue, &mut sources));
    fs::create_dir_all(dir.path("elsewhere")).unwrap();
    fs::write(dir.path("elsewhere/log"), "step delta\n").unwrap();
    fs::write(dir.path("logs/undeclared.log"), "step epsilon\n").unwrap();
    assert!(!scan(&queue, &mut sources));
    drop(open);
}

#[test]
fn defer_live_and_gone() {
    let dir = TempDir::new();
    let mut live = Waiting::start();
    let mut gone = Waiting::start();
    gone.stop();
    assert_eq!(gone.reap(), 0);
    fs::create_dir(dir.path("registry")).unwrap();
    for name in [gone.pid_text(), "invalid".into(), "0".into()] {
        fs::write(dir.path(&format!("registry/{name}")), "").unwrap();
    }
    let registry = dir.text("registry");
    assert!(!judging_elsewhere(&mut PosixFiles, &registry).unwrap());
    fs::write(dir.path(&format!("registry/{}", live.pid())), "").unwrap();
    assert!(judging_elsewhere(&mut PosixFiles, &registry).unwrap());
    live.stop();
    assert_eq!(live.reap(), 0);
    assert!(!judging_elsewhere(&mut PosixFiles, &registry).unwrap());
    assert!(!judging_elsewhere(&mut PosixFiles, &dir.text("missing")).unwrap());
}
