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

fn isolated(dir: &TempDir, source: &str) -> Command {
    let mut command = script(dir, source);
    for (key, _) in std::env::vars() {
        if key.starts_with("CAIM_ARG_") || key == "CAIM_ROOT" {
            command.env_remove(key);
        }
    }
    command.env_remove("CAIM_ARG_DIR");
    command
}

#[test]
fn usage_ends_the_script_with_64_from_any_coroutine() {
    let dir = TempDir::new();
    let (status, output) = run(&mut isolated(&dir, "caim.usage('usage: build FILE')\nprint('not reached')"));
    assert_eq!((status, output.as_str()), (64, "usage: build FILE\n"));
    let source = "caim.join(caim.go(function() caim.usage('from a coroutine') end))\nprint('not reached')";
    assert_eq!(run(&mut isolated(&dir, source)), (64, "from a coroutine\n".into()));
}

#[test]
fn flags_turn_non_empty_arguments_into_options_in_a_fixed_order() {
    let dir = TempDir::new();
    let source = r#"
        local args = { platform = "Win64", name = "a b", clean = "", absent = nil }
        local words = caim.flags(args, { platform = "--platform", name = "--name", clean = "--clean", absent = "--absent" })
        print(#words, table.concat(words, "|"))
        print(#caim.flags({}, {}))
        print(pcall(caim.flags, args, { name = {} }))
    "#;
    let (status, output) = run(&mut isolated(&dir, source));
    assert_eq!(status, 0, "{output}");
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("4\t--name|a b|--platform|Win64"));
    assert_eq!(lines.next(), Some("0"));
    assert!(lines.next().is_some_and(|line| line.starts_with("false\t")), "{output}");
}

#[test]
fn env_takes_the_argument_then_the_variable_then_the_default() {
    let dir = TempDir::new();
    let source = r#"print(caim.env("engine", "default"), caim.env("two-words"), caim.env("TWO_WORDS", "d"))"#;
    let (status, output) = run(&mut isolated(&dir, source));
    assert_eq!((status, output.as_str()), (0, "default\tnil\td\n"));
    let (_, output) = run(isolated(&dir, source).env("ENGINE", "from-variable").env("TWO_WORDS", "var"));
    assert_eq!(output, "from-variable\tvar\tvar\n");
    let (_, output) =
        run(isolated(&dir, source).env("ENGINE", "from-variable").env("CAIM_ARG_ENGINE", "from-argument"));
    assert_eq!(output, "from-argument\tnil\td\n");
    let (_, output) = run(isolated(&dir, source).env("ENGINE", "from-variable").env("CAIM_ARG_ENGINE", ""));
    assert_eq!(output, "from-variable\tnil\td\n");
    let (_, output) = run(isolated(&dir, source).env("CAIM_ARG_TWO_WORDS", "arg").env("TWO_WORDS", "var"));
    assert_eq!(output, "default\targ\targ\n");
}

#[test]
fn root_is_the_project_and_abs_resolves_against_the_current_directory() {
    let project = TempDir::new();
    fs::write(project.path("caim.toml"), "name = \"case\"\n").unwrap();
    fs::create_dir_all(project.path("sub/deeper")).unwrap();
    let source = "print(caim.root, caim.abs('x/../y.log'), caim.abs('/a/b'), pcall(caim.abs, ''))";
    let (status, output) = run(isolated(&project, source).current_dir(project.path("sub/deeper")));
    let sub = project.text("sub/deeper");
    let first = format!("{}\t{sub}/y.log\t/a/b\tfalse\t", project.0.display());
    assert!(status == 0 && output.starts_with(&first), "{output}");
    let (_, output) = run(isolated(&project, "print(caim.root)").env("CAIM_ROOT", "/elsewhere"));
    assert_eq!(output, "/elsewhere\n");
    let bare = TempDir::new();
    let (_, output) = run(isolated(&bare, "print(caim.root)").current_dir(&bare.0));
    assert_eq!(output, format!("{}\n", bare.0.display()));
}

fn blocked_child(dir: &TempDir, name: &str) -> String {
    fifo(dir, &format!("{name}.fifo"));
    format!(
        "caim.spawn{{ 'sh', '-c', 'trap \"\" TERM; echo ready; read x < {0}/{1}.fifo', output = '{0}/{1}.log' }}",
        dir.0.display(),
        name
    )
}

#[test]
fn first_is_the_first_of_several_to_exit() {
    let dir = TempDir::new();
    let source = format!(
        r#"
        io.stdout:setvbuf("line")
        local a, b = {}, {}
        print("started")
        local first = caim.first{{ a, b }}
        print(first == b, first == a)
        print(caim.exit(b))
        local again = caim.first{{ a, b }}
        print(again == b)
        caim.stop(a, {{ "KILL:30" }})
        print(caim.exit(a))
        "#,
        blocked_child(&dir, "a"),
        blocked_child(&dir, "b")
    );
    let mut child = spawn(&mut script(&dir, &source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "started\n");
    release(&dir, "b.fifo");
    let (status, output) = finish(child);
    assert_eq!(status, 0, "{output}");
    assert!(output.ends_with("true\tfalse\n0\ntrue\n137\n"), "{output}");
}

#[test]
fn stop_escalates_through_its_steps() {
    let dir = TempDir::new();
    let source = format!(
        r#"
        local pid = {}
        assert(caim.line(caim.args.dir .. "/a.log", "^ready$", pid))
        print(caim.stop(pid, {{ "TERM:0", "KILL:30" }}))
        print(caim.exit(pid))
        print(pcall(caim.stop, pid, {{ "USR1:1" }}))
        "#,
        blocked_child(&dir, "a")
    );
    let (status, output) = run(&mut script(&dir, &source));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("[stop] TERM") && output.contains("[stop] KILL"), "{output}");
    assert!(output.contains("true\n137\nfalse\t"), "{output}");
}

#[test]
fn run_reports_how_a_command_ended() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/out.log"
        print(caim.run{ "sh", "-c", "echo hello; exit 3", output = log, idle = 60 })
        print(io.open(log):read("a"):gsub("\n", ""))
        print(pcall(caim.run, { "true", idle = 1 }))
        print(pcall(caim.run, { "true", output = log }))
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("exit\t3"));
    assert_eq!(lines.next(), Some("hello\t1"));
    assert!(output.contains("run needs output") && output.contains("run needs idle"), "{output}");
}

#[test]
fn run_stops_a_command_that_stops_making_progress() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/out.log"
        print(caim.run{ "sh", "-c", "echo started; exec sleep 1000", output = log, idle = 1, steps = { "TERM:30", "KILL:30" } })
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("no progress for 1 s"), "{output}");
    assert!(output.ends_with("stall\t143\n"), "{output}");
}

#[test]
fn deferred_functions_run_last_in_reverse_order_whatever_ends_the_script() {
    let dir = TempDir::new();
    let note =
        |text: &str| format!("local f = io.open(caim.args.dir .. '/notes', 'a'); f:write('{text}\\n'); f:close()");
    for (body, expected, notes) in [
        ("return 4", 4, "second\nfirst\n"),
        ("error('boom')", 125, "second\nfirst\n"),
        ("caim.usage('usage: x')", 64, "second\nfirst\n"),
        ("caim.join(caim.go(function() caim.join(0) end))", 125, "second\nfirst\n"),
    ] {
        let _ = fs::remove_file(dir.path("notes"));
        let source = format!(
            "caim.defer(function() {} end)\ncaim.defer(function() {} end)\n{body}",
            note("first"),
            note("second")
        );
        let (status, output) = run(&mut script(&dir, &source));
        assert_eq!(status, expected, "{body}: {output}");
        assert_eq!(fs::read_to_string(dir.path("notes")).unwrap(), notes, "{body}");
    }
}

#[test]
fn a_deferred_function_cannot_wait_and_a_failing_one_does_not_stop_the_rest() {
    let dir = TempDir::new();
    let source = r#"
        caim.defer(function() print("still ran") end)
        caim.defer(function() caim.exit(1) end)
        return 0
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("deferred function failed") && output.contains("cannot wait"), "{output}");
    assert!(output.contains("still ran"), "{output}");
}

#[test]
fn a_termination_signal_runs_the_deferred_functions_then_ends_with_128_plus_it() {
    let dir = TempDir::new();
    let source = format!(
        r#"
        io.stdout:setvbuf("line")
        local pid = {}
        caim.defer(function()
            caim.stop(pid, {{ "TERM:0", "KILL:30" }})
            local f = io.open(caim.args.dir .. "/cleaned", "w"); f:write("yes"); f:close()
        end)
        assert(caim.line(caim.args.dir .. "/a.log", "^ready$", pid))
        print("ready")
        caim.exit(pid)
        print("not reached")
        "#,
        blocked_child(&dir, "a")
    );
    let mut child = spawn(&mut script(&dir, &source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "ready\n");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let (status, output) = finish(child);
    assert_eq!(status, 128 + libc::SIGTERM, "{output}");
    assert!(!output.contains("not reached"), "{output}");
    assert_eq!(fs::read_to_string(dir.path("cleaned")).unwrap(), "yes");
}
