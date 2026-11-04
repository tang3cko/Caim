mod common;
use common::*;
use std::fs;

#[test]
fn exit_and_first_exit() {
    let mut first = Waiting::start();
    let mut second = Waiting::start();
    let waiter =
        spawn(caim().args(["first-exit", "--pid", &first.pid_text(), "--pid", &second.pid_text(), "--guard", "180"]));
    second.stop();
    let (status, output) = finish(waiter);
    assert_eq!((status, output.as_str()), (0, format!("exit {}\n", second.pid()).as_str()));
    assert_eq!(second.reap(), 0);
    assert!(alive(first.pid()), "survivor exited");
    let (status, output) = run(caim().args(["first-exit", "--pid", &first.pid_text(), "--pid", &second.pid_text()]));
    assert_eq!(status, 0);
    assert!(output.contains(&format!("exit {}", second.pid())));
    assert_eq!(run(caim().args(["exit", &second.pid_text()])).0, 0);
    let (status, output) = run(caim().args(["exit", &first.pid_text(), "0"]));
    assert_eq!(status, 124, "a hang guard that runs out is a timeout: {output}");
    let waiter = spawn(caim().args(["exit", &first.pid_text(), "180"]));
    first.stop();
    assert_eq!(finish(waiter).0, 0);
    assert_eq!(first.reap(), 0);
}

#[test]
fn written_and_lines() {
    let dir = TempDir::new();
    let log = dir.path("log");
    fs::write(&log, "").unwrap();
    let waiter = spawn(caim().args(["written", &dir.text("log"), "180"]));
    fs::write(&log, "first\n").unwrap();
    assert_eq!(finish(waiter).0, 0);
    assert_eq!(run(caim().args(["written", &dir.text("log")])).0, 0);
    let (status, output) = run(caim().args(["line", "--match", &dir.text("log"), "^first$"]));
    assert_eq!((status, output.as_str()), (0, "match 0\n"));

    let mut producer = Waiting::start();
    fs::write(&log, "wanted\ngate and wanted\n").unwrap();
    fs::write(dir.path("other"), "").unwrap();
    let waiter = spawn(caim().args([
        "line",
        "--guard",
        "180",
        "--pid",
        &producer.pid_text(),
        "--after",
        &dir.text("log"),
        "gate",
        "^wanted$",
        "--match",
        &dir.text("other"),
        "absent",
    ]));
    append(&log, b"wan");
    append(&log, b"ted\n");
    producer.stop();
    assert_eq!(producer.reap(), 0);
    let (status, output) = finish(waiter);
    assert_eq!((status, output.as_str()), (0, "match 1\n"));
    assert_eq!(run(caim().args(["line", "--pid", &producer.pid_text(), "--match", &dir.text("log"), "^wanted$"])).0, 0);
    let (status, output) =
        run(caim().args(["line", "--pid", &producer.pid_text(), "--match", &dir.text("log"), "never present"]));
    assert_eq!(status, 1);
    assert!(output.contains(&format!("exit {}", producer.pid())));
    let args = ["line", "--pid", &producer.pid_text(), "--after", &dir.text("log"), "^wanted$", "^gate and wanted$"];
    assert_eq!(run(caim().args(args)).0, 0);
}

#[test]
fn line_gate_strict() {
    let dir = TempDir::new();
    let mut producer = Waiting::start();
    fs::write(dir.path("log"), "wanted\ngate wanted\n").unwrap();
    producer.stop();
    assert_eq!(producer.reap(), 0);
    let (status, _) =
        run(caim().args(["line", "--pid", &producer.pid_text(), "--after", &dir.text("log"), "gate", "wanted"]));
    assert_eq!(status, 1);
}

#[test]
fn activity_live_and_gone() {
    let dir = TempDir::new();
    let mut victim = Waiting::start();
    fs::create_dir(dir.path("registry")).unwrap();
    fs::write(dir.path("ready"), "").unwrap();
    let watcher = spawn(caim().args([
        "activity",
        "--pid",
        &victim.pid_text(),
        "--idle",
        "180",
        "--log",
        &dir.text("missing/run.log"),
        "--ready",
        &dir.text("ready"),
        "--register",
        &dir.text("registry"),
    ]));
    written(&dir.path("ready"));
    let registration = dir.path(&format!("registry/{}", watcher.id()));
    assert!(registration.exists(), "registration missing");
    victim.stop();
    assert_eq!(victim.reap(), 0);
    assert_eq!(finish(watcher).0, 0);
    assert!(!registration.exists(), "registration leaked after exit");
    let args = ["activity", "--pid", &victim.pid_text(), "--idle", "180", "--log", &dir.text("run.log")];
    assert_eq!(run(caim().args(args)).0, 0);
}

#[test]
fn activity_registration_cleanup() {
    let dir = TempDir::new();
    let mut victim = Waiting::start();
    fs::create_dir(dir.path("registry")).unwrap();
    fs::write(dir.path("ready"), "").unwrap();
    let watcher = spawn(caim().args([
        "activity",
        "--pid",
        &victim.pid_text(),
        "--idle",
        "180",
        "--log",
        &dir.text("run.log"),
        "--ready",
        &dir.text("ready"),
        "--register",
        &dir.text("registry"),
    ]));
    written(&dir.path("ready"));
    let registration = dir.path(&format!("registry/{}", watcher.id()));
    unsafe { libc::kill(watcher.id() as i32, libc::SIGTERM) };
    assert_eq!(finish(watcher).0, 143);
    assert!(!registration.exists(), "registration leaked after TERM");
    assert!(alive(victim.pid()), "watcher killed victim");
    victim.stop();
    assert_eq!(victim.reap(), 0);
}

#[test]
fn activity_declarations() {
    let dir = TempDir::new();
    let mut victim = Waiting::start();
    fs::write(dir.path("watch"), "").unwrap();
    fs::write(dir.path("ready"), "").unwrap();
    let watcher = spawn(caim().args([
        "activity",
        "--pid",
        &victim.pid_text(),
        "--idle",
        "180",
        "--declared",
        &dir.text("watch"),
        "--ready",
        &dir.text("ready"),
    ]));
    written(&dir.path("ready"));
    append(&dir.path("watch"), format!("tree {}\n", dir.0.display()).as_bytes());
    let (status, output) = finish(watcher);
    assert_eq!(status, 125);
    assert!(output.contains("the watch failed"), "{output}");
    assert!(output.contains("unknown watch declaration"), "{output}");
    assert!(!output.contains("no progress for"), "{output}");
    assert!(alive(victim.pid()), "coverage failure killed victim");
    fs::write(dir.path("watch"), "log relative.log\n").unwrap();
    let args = ["activity", "--pid", &victim.pid_text(), "--idle", "180", "--declared", &dir.text("watch")];
    let (status, output) = run(caim().args(args));
    assert_eq!(status, 125);
    assert!(output.contains("absolute path"), "{output}");
    victim.stop();
    assert_eq!(victim.reap(), 0);
}

#[test]
fn stop_steps() {
    let mut obeys = Waiting::shell("IFS= read -r stop", true);
    let (status, output) =
        run(caim().args(["stop", "--group", &obeys.pid_text(), "--step", "TERM:180", "--step", "KILL:180"]));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains(&format!("[stop] TERM to 1 process(es): {}", obeys.pid())), "{output}");
    assert!(!output.contains("[stop] KILL"), "{output}");
    assert_eq!(obeys.reap(), 143);

    let mut ignores = Waiting::shell("trap '' TERM; echo trapped; IFS= read -r stop", true);
    assert_eq!(stdout_line(ignores.child.stdout.as_mut().unwrap()), "trapped\n");
    let (status, output) =
        run(caim().args(["stop", "--group", &ignores.pid_text(), "--step", "TERM:1", "--step", "KILL:180"]));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("[stop] TERM to "), "{output}");
    assert!(output.contains("[stop] KILL to "), "{output}");
    assert_eq!(ignores.reap(), 137);
    let (status, output) = run(caim().args(["stop", "--pid", &ignores.pid_text(), "--step", "TERM:180"]));
    assert_eq!(status, 0);
    assert!(!output.contains("[stop]"), "{output}");
}

#[test]
fn stop_counts_a_zombie_as_gone() {
    let mut zombie = Waiting::shell("IFS= read -r stop", true);
    zombie.stop();
    assert_eq!(run(caim().args(["exit", &zombie.pid_text(), "180"])).0, 0);
    let (status, output) = run(caim().args(["stop", "--group", &zombie.pid_text(), "--step", "TERM:180"]));
    assert_eq!(status, 0, "{output}");
    assert!(!output.contains("[stop]"), "{output}");
    assert_eq!(zombie.reap(), 0);
}

#[test]
fn cli_usage() {
    let dir = TempDir::new();
    let log = dir.text("log");
    let result = dir.text("result");
    fs::write(&result, "").unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["activity", "--pid", "1", "--idle", "0", "--log", &log],
        vec!["activity", "--pid", "1", "--idle", "180"],
        vec!["activity", "--pid", "1", "--idle", "180", "--tree", &log],
        vec!["stop", "--step", "TERM:1"],
        vec!["stop", "--group", "1", "--step", "BOGUS:1"],
        vec!["stop", "--group", "1"],
        vec!["first-exit", "--pid", "1", "--pid", "1"],
        vec!["line"],
        vec!["exit", "0"],
        vec!["written", &result, "nan"],
        vec!["run", "--idle", "180", "--output", &log],
        vec!["line", "--match", &result, "("],
    ];
    for case in cases {
        let (status, output) = run(caim().args(&case));
        assert_eq!(status, 64, "{case:?}: {output}");
        assert!(output.starts_with(&format!("caim {}: ", case[0])), "{case:?}: {output}");
    }
}

#[test]
fn run_records_the_exit() {
    let dir = TempDir::new();
    let args = [
        "run",
        "--idle",
        "180",
        "--output",
        &dir.text("out.log"),
        "--outcome",
        &dir.text("outcome"),
        "--step",
        "TERM:20",
        "--",
        "/bin/sh",
        "-c",
        "printf '%s ' $$; perl -e 'print getpgrp(), qq{\\n}; exit 7'",
    ];
    let (status, output) = run(caim().args(args));
    assert_eq!(status, 0, "{output}");
    let pid = output.trim().strip_prefix("[run] pid ").unwrap().to_string();
    assert_eq!(fs::read_to_string(dir.path("outcome")).unwrap(), "exit 7\n");
    let logged = fs::read_to_string(dir.path("out.log")).unwrap();
    let words: Vec<&str> = logged.split_whitespace().collect();
    assert_eq!(words, vec![pid.as_str(), pid.as_str()], "the command leads its own group");
}

#[test]
fn run_stops_a_silent_command() {
    let dir = TempDir::new();
    let args = [
        "run",
        "--idle",
        "1",
        "--output",
        &dir.text("out.log"),
        "--outcome",
        &dir.text("outcome"),
        "--step",
        "TERM:180",
        "--",
        "/bin/sh",
        "-c",
        "echo started; exec tail -f /dev/null",
    ];
    let (status, output) = run(caim().args(args));
    assert_eq!(status, 124, "{output}");
    assert!(output.contains("no progress for 1 s"), "{output}");
    assert!(output.contains("[stop] TERM to 1 process(es)"), "{output}");
    assert_eq!(fs::read_to_string(dir.path("outcome")).unwrap(), "stall status=143\n");
}
