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
fn the_return_value_is_the_status() {
    let dir = TempDir::new();
    assert_eq!(run(&mut script(&dir, "return 3")).0, 3);
    assert_eq!(run(&mut script(&dir, "print('hello')")), (0, "hello\n".into()));
}

#[test]
fn arguments_and_the_task_name() {
    let dir = TempDir::new();
    let source = "print(caim.args.tool, caim.args.two_words, caim.task)";
    let (status, output) = run(script(&dir, source)
        .env("CAIM_ARG_TOOL", "alpha")
        .env("CAIM_ARG_TWO_WORDS", "x")
        .env("CAIM_TASK", "build"));
    assert_eq!((status, output.as_str()), (0, "alpha\tx\tbuild\n"));
}

#[test]
fn the_exit_status_of_a_process() {
    let dir = TempDir::new();
    let source = r#"
        local exited = caim.spawn{ "sh", "-c", "exit 7" }
        local killed = caim.spawn{ "sh", "-c", "kill -KILL $$" }
        print(caim.exit(exited), caim.exit(killed))
    "#;
    assert_eq!(run(&mut script(&dir, source)), (0, "7\t137\n".into()));
}

#[test]
fn a_line_that_never_comes_ends_with_its_process() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/run.log"
        local pid = caim.spawn{ "sh", "-c", "echo other", output = log }
        print(caim.line(log, "^ALL DONE$", pid))
        pid = caim.spawn{ "sh", "-c", "echo ALL DONE", output = log }
        print(caim.line(log, "^ALL DONE$", pid))
    "#;
    assert_eq!(run(&mut script(&dir, source)), (0, "false\ntrue\n".into()));
}

#[test]
fn waits_run_at_once_and_the_first_event_wins() {
    let dir = TempDir::new();
    fifo(&dir, "alpha.fifo");
    fifo(&dir, "beta.fifo");
    let source = r#"
        io.stdout:setvbuf("line")
        local dir = caim.args.dir
        local function case(name)
            local log = dir .. "/" .. name .. ".log"
            local command = "read x < " .. dir .. "/" .. name .. ".fifo; echo ALL DONE " .. name
            local pid = caim.spawn{ "sh", "-c", command, output = log }
            return caim.go(function()
                local seen = caim.line(log, "^ALL DONE " .. name, pid)
                print(name, seen, caim.exit(pid))
                return seen
            end)
        end
        local alpha, beta = case("alpha"), case("beta")
        print("started")
        print(caim.join(alpha), caim.join(beta))
    "#;
    let mut child = spawn(&mut script(&dir, source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "started\n");
    release(&dir, "beta.fifo");
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "beta\ttrue\t0\n");
    release(&dir, "alpha.fifo");
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "alpha\ttrue\t0\n");
    assert_eq!(finish(child), (0, "true\ttrue\n".into()));
}

#[test]
fn a_line_in_a_file_that_does_not_exist_yet() {
    let dir = TempDir::new();
    fifo(&dir, "step.fifo");
    fs::create_dir(dir.path("marks")).unwrap();
    fs::write(dir.path("marks/one"), "").unwrap();
    fs::write(dir.path("marks/two"), "").unwrap();
    // The helper makes the directories one at a time, marking each, and writes the line last.
    let source = r#"
        io.stdout:setvbuf("line")
        local dir = caim.args.dir
        local step = "read x < " .. dir .. "/step.fifo"
        local helper = table.concat({
            step, "mkdir " .. dir .. "/a", "echo made >> " .. dir .. "/marks/one",
            step, "mkdir " .. dir .. "/a/b", "echo made >> " .. dir .. "/marks/two",
            step, "echo ALL DONE > " .. dir .. "/a/b/run.log",
        }, "; ")
        caim.spawn{ "sh", "-c", helper }
        local waiting = caim.go(function() return caim.line(dir .. "/a/b/run.log", "^ALL DONE") end)
        caim.go(function() print("parked") end)
        print(caim.join(waiting))
    "#;
    let mut child = spawn(&mut script(&dir, source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "parked\n");
    release(&dir, "step.fifo");
    written(&dir.path("marks/one"));
    release(&dir, "step.fifo");
    written(&dir.path("marks/two"));
    release(&dir, "step.fifo");
    assert_eq!(finish(child), (0, "true\n".into()));
}

#[test]
fn a_reaped_process_is_gone_for_later_waits() {
    let dir = TempDir::new();
    fifo(&dir, "alpha.fifo");
    fs::write(dir.path("run.log"), "").unwrap();
    // Two waiters on one process: the exit wait reaps it, and the line wait must still end.
    let source = r#"
        io.stdout:setvbuf("line")
        local dir = caim.args.dir
        local log = dir .. "/run.log"
        local pid = caim.spawn{ "sh", "-c", "read x < " .. dir .. "/alpha.fifo" }
        local line = caim.go(function() return caim.line(log, "^ALL DONE", pid) end)
        local exit = caim.go(function() return caim.exit(pid) end)
        print("started")
        print(caim.join(line), caim.join(exit))
        print(caim.line(log, "^ALL DONE", pid), caim.exit(pid))
    "#;
    let mut child = spawn(&mut script(&dir, source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "started\n");
    release(&dir, "alpha.fifo");
    assert_eq!(finish(child), (0, "false\t0\nfalse\tnil\n".into()));
}

#[test]
fn declare_appends_a_progress_source_when_caim_offers_a_file() {
    let dir = TempDir::new();
    let source = r#"print(caim.declare("log", "run.log"))"#;
    assert_eq!(run(&mut script(&dir, source)), (0, "false\n".into()));
    let declared = dir.path("declared");
    fs::write(&declared, "").unwrap();
    let (status, output) = run(script(&dir, source).env("CAIM_DECLARED", &declared).current_dir(&dir.0));
    assert_eq!((status, output.as_str()), (0, "true\n"));
    assert_eq!(fs::read_to_string(&declared).unwrap(), format!("log {}\n", dir.text("run.log")));
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
