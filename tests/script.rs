mod common;
use common::*;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::process::Command;

fn script(dir: &TempDir, source: &str) -> Command {
    fs::write(dir.path("main.lua"), source).unwrap();
    let mut command = caim();
    command.args(["script", &dir.text("main.lua")]).env("CAIM_ARG_DIR", &dir.0);
    command
}

fn fifo(dir: &TempDir, name: &str) {
    let path = CString::new(dir.text(name)).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

fn release(dir: &TempDir, name: &str) {
    fs::OpenOptions::new().write(true).open(dir.path(name)).unwrap().write_all(b"go\n").unwrap();
}

#[test]
fn the_script_status_is_the_return_value() {
    let dir = TempDir::new();
    assert_eq!(run(&mut script(&dir, "return 3")).0, 3);
    assert_eq!(run(&mut script(&dir, "print('hello')")), (0, "hello\n".into()));
}

#[test]
fn arguments_and_task_name() {
    let dir = TempDir::new();
    let source = "print(caim.args.platform, caim.args.two_words, caim.task)";
    let (status, output) = run(script(&dir, source)
        .env("CAIM_ARG_PLATFORM", "Mac")
        .env("CAIM_ARG_TWO_WORDS", "x")
        .env("CAIM_TASK", "build"));
    assert_eq!((status, output.as_str()), (0, "Mac\tx\tbuild\n"));
}

#[test]
fn exit_status_of_a_process() {
    let dir = TempDir::new();
    let source = r#"
        local exited = caim.spawn{ "sh", "-c", "exit 7" }
        local killed = caim.spawn{ "sh", "-c", "kill -KILL $$" }
        print(caim.exit(exited), caim.exit(killed))
        return 0
    "#;
    assert_eq!(run(&mut script(&dir, source)), (0, "7\t137\n".into()));
}

#[test]
fn a_line_that_never_comes_ends_with_its_process() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/log"
        local pid = caim.spawn{ "sh", "-c", "echo other", output = log }
        print(caim.line(log, "^wanted$", pid))
        local pid = caim.spawn{ "sh", "-c", "echo wanted", output = log }
        print(caim.line(log, "^wanted$", pid))
    "#;
    assert_eq!(run(&mut script(&dir, source)), (0, "false\ntrue\n".into()));
}

#[test]
fn waits_run_at_once_and_the_first_event_wins() {
    let dir = TempDir::new();
    fifo(&dir, "a.fifo");
    fifo(&dir, "b.fifo");
    let source = r#"
        io.stdout:setvbuf("line")
        local dir = caim.args.dir
        local function ready(name)
            local log = dir .. "/" .. name .. ".log"
            local pid = caim.spawn{ "sh", "-c", "read x < " .. dir .. "/" .. name .. ".fifo; echo ready-" .. name, output = log }
            return caim.go(function()
                local seen = caim.line(log, "^ready-" .. name, pid)
                print(name, seen, caim.exit(pid))
                return seen
            end)
        end
        local a, b = ready("a"), ready("b")
        print("started")
        print(caim.join(a), caim.join(b))
    "#;
    let mut child = spawn(&mut script(&dir, source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "started\n");
    release(&dir, "b.fifo");
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "b\ttrue\t0\n");
    release(&dir, "a.fifo");
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "a\ttrue\t0\n");
    assert_eq!(finish(child), (0, "true\ttrue\n".into()));
}

#[test]
fn declare_appends_a_progress_source_when_caim_offers_a_file() {
    let dir = TempDir::new();
    let source = r#"print(caim.declare("log", "out.log"))"#;
    assert_eq!(run(&mut script(&dir, source)), (0, "false\n".into()));
    let declared = dir.path("declared");
    fs::write(&declared, "").unwrap();
    let (status, output) = run(script(&dir, source).env("CAIM_DECLARED", &declared).current_dir(&dir.0));
    assert_eq!((status, output.as_str()), (0, "true\n"));
    assert_eq!(fs::read_to_string(&declared).unwrap(), format!("log {}\n", dir.text("out.log")));
    let (status, output) = run(&mut script(&dir, r#"caim.declare("socket", "x")"#));
    assert_eq!(status, 125);
    assert!(output.contains(r#"declare takes "log" or "dir""#), "{output}");
}

#[test]
fn failures_are_reported_not_hidden() {
    let dir = TempDir::new();
    let (status, output) = run(&mut script(&dir, "error('boom')"));
    assert_eq!(status, 125);
    assert!(output.contains("caim script:") && output.contains("boom"), "{output}");
    let (status, output) = run(&mut script(&dir, "local x = = 1"));
    assert_eq!(status, 125);
    assert!(output.contains("caim script:"), "{output}");
    let (status, output) = run(caim().args(["script", &dir.text("absent.lua")]));
    assert_eq!(status, 125);
    assert!(output.contains("absent.lua"), "{output}");
    assert_eq!(run(caim().args(["script"])).0, 64);
    let (status, output) = run(&mut script(&dir, "caim.join(caim.go(function() caim.join(0) end))"));
    assert_eq!(status, 125);
    assert!(output.contains("nothing can wake"), "{output}");
}

#[test]
fn script_is_not_a_task_name() {
    let dir = TempDir::new();
    fs::write(dir.path("caim.toml"), "name = \"case\"\n[tasks.script]\nrun = \"true\"\n").unwrap();
    let (status, output) = run(caim().arg("list").env("CAIM_ROOT", &dir.0));
    assert_eq!(status, 78, "{output}");
}
