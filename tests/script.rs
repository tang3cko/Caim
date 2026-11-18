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
fn an_error_names_the_file_and_line() {
    let dir = TempDir::new();
    // Run from the script's directory: Lua cuts a long name down to its end, and a temporary
    // directory's path is long enough on macOS to lose its start.
    let source = "local function fail()\n  error('boom')\nend\nfail()\n";
    let (status, output) = run(script(&dir, source).current_dir(&dir.0));
    assert_eq!(status, 125);
    assert!(output.starts_with("caim script: runtime error: main.lua:2: boom\n"), "{output}");
    assert!(output.contains("stack traceback:") && !output.contains("[string"), "{output}");
    let (status, output) = run(script(&dir, "local x = = 1").current_dir(&dir.0));
    assert_eq!(status, 125);
    assert!(output.contains(": main.lua:1:") && !output.contains("[string"), "{output}");
}

/// A script with none of the caller's CAIM_ variables.
fn isolated(dir: &TempDir, source: &str) -> Command {
    let mut command = script(dir, source);
    for (key, _) in std::env::vars() {
        if key.starts_with("CAIM_") {
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
fn flags_turn_the_non_empty_arguments_into_options_in_name_order() {
    let dir = TempDir::new();
    let source = r#"
        local args = { tool = "alpha", name = "a b", clean = "" }
        local words = caim.flags(args, { tool = "--tool", name = "--name", clean = "--clean", absent = "--absent" })
        print(#words, table.concat(words, "|"))
        print(#caim.flags({}, {}))
        print(pcall(caim.flags, args, { name = {} }))
    "#;
    let (status, output) = run(&mut isolated(&dir, source));
    assert_eq!(status, 0, "{output}");
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("4\t--name|a b|--tool|alpha"));
    assert_eq!(lines.next(), Some("0"));
    assert!(lines.next().is_some_and(|line| line.starts_with("false\t")), "{output}");
}

#[test]
fn env_takes_the_argument_then_the_variable_then_the_default() {
    let dir = TempDir::new();
    let source = r#"print(caim.env("tool", "default"), caim.env("two-words"))"#;
    assert_eq!(run(&mut isolated(&dir, source)).1, "default\tnil\n");
    assert_eq!(run(isolated(&dir, source).env("TOOL", "variable").env("TWO_WORDS", "v")).1, "variable\tv\n");
    let (_, output) = run(isolated(&dir, source).env("TOOL", "variable").env("CAIM_ARG_TOOL", "argument"));
    assert_eq!(output, "argument\tnil\n");
    let (_, output) = run(isolated(&dir, source).env("TOOL", "variable").env("CAIM_ARG_TOOL", ""));
    assert_eq!(output, "variable\tnil\n", "an empty argument counts as none");
}

#[test]
fn root_is_the_project_and_abs_resolves_against_the_current_directory() {
    let project = TempDir::new();
    fs::write(project.path("caim.toml"), "name = \"case\"\n").unwrap();
    fs::create_dir_all(project.path("sub/deeper")).unwrap();
    let source = "print(caim.root, caim.abs('x/../y.log'), caim.abs('/a/b'), (pcall(caim.abs, '')))";
    let (status, output) = run(isolated(&project, source).current_dir(project.path("sub/deeper")));
    let expected = format!("{}\t{}/y.log\t/a/b\tfalse\n", project.0.display(), project.text("sub/deeper"));
    assert_eq!((status, output), (0, expected));
    let (_, output) = run(isolated(&project, "print(caim.root)").env("CAIM_ROOT", "/elsewhere"));
    assert_eq!(output, "/elsewhere\n");
    let bare = TempDir::new();
    let (_, output) = run(isolated(&bare, "print(caim.root)").current_dir(&bare.0));
    assert_eq!(output, format!("{}\n", bare.0.display()));
}

/// Lua that starts a process ignoring TERM, which says ready and waits on NAME.fifo.
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
        local alpha, beta = {}, {}
        print("started")
        local first = caim.first{{ alpha, beta }}
        print(first == beta, first == alpha)
        print(caim.exit(beta))
        caim.stop(alpha, {{ "KILL:30" }})
        print(caim.exit(alpha))
        "#,
        blocked_child(&dir, "alpha"),
        blocked_child(&dir, "beta")
    );
    let mut child = spawn(&mut script(&dir, &source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "started\n");
    release(&dir, "beta.fifo");
    let (status, output) = finish(child);
    assert_eq!(status, 0, "{output}");
    assert!(output.ends_with("true\tfalse\n0\n137\n"), "{output}");
}

#[test]
fn stop_escalates_through_its_steps() {
    let dir = TempDir::new();
    let source = format!(
        r#"
        local pid = {}
        assert(caim.line(caim.args.dir .. "/alpha.log", "^ready$", pid))
        print(caim.stop(pid, {{ "TERM:0", "KILL:30" }}))
        print(caim.exit(pid))
        print(pcall(caim.stop, pid, {{ "USR1:1" }}))
        "#,
        blocked_child(&dir, "alpha")
    );
    let (status, output) = run(&mut script(&dir, &source));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("true\n137\nfalse\t"), "{output}");
}

#[test]
fn run_reports_how_a_command_ended() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/run.log"
        print(caim.run{ "sh", "-c", "echo hello; exit 3", output = log, idle = 60 })
        print((io.open(log):read("a"):gsub("\n", "")))
        print(pcall(caim.run, { "true", idle = 1 }))
        print(pcall(caim.run, { "true", output = log }))
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    let mut lines = output.lines();
    assert_eq!(lines.next(), Some("exit\t3"));
    assert_eq!(lines.next(), Some("hello"));
    assert!(output.contains("run needs output") && output.contains("run needs idle"), "{output}");
}

#[test]
fn run_stops_a_command_that_stops_making_progress() {
    let dir = TempDir::new();
    let source = r#"
        local log = caim.args.dir .. "/run.log"
        print(caim.run{ "sh", "-c", "echo started; exec tail -f /dev/null", output = log, idle = 1, steps = { "TERM:30", "KILL:30" } })
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    assert!(output.ends_with("stall\t143\n"), "{output}");
}

#[test]
fn deferred_functions_run_last_first_whatever_ends_the_script() {
    let dir = TempDir::new();
    let note =
        |text: &str| format!("local f = io.open(caim.args.dir .. '/notes', 'a'); f:write('{text}\\n'); f:close()");
    for (body, expected) in [
        ("return 4", 4),
        ("error('boom')", 125),
        ("caim.usage('usage: x')", 64),
        ("caim.join(caim.go(function() caim.join(0) end))", 125),
    ] {
        let _ = fs::remove_file(dir.path("notes"));
        let source = format!(
            "caim.defer(function() {} end)\ncaim.defer(function() {} end)\n{body}",
            note("first"),
            note("second")
        );
        let (status, output) = run(&mut script(&dir, &source));
        assert_eq!(status, expected, "{body}: {output}");
        assert_eq!(fs::read_to_string(dir.path("notes")).unwrap(), "second\nfirst\n", "{body}");
    }
}

#[test]
fn a_deferred_function_cannot_wait_and_a_failing_one_does_not_stop_the_rest() {
    let dir = TempDir::new();
    let source = r#"
        caim.defer(function() print("still ran") end)
        caim.defer(function() caim.exit(1) end)
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("deferred function failed") && output.contains("cannot wait"), "{output}");
    assert!(output.contains("still ran"), "{output}");
}

#[test]
fn a_termination_signal_after_a_run_still_runs_the_deferred_functions() {
    let dir = TempDir::new();
    let source = format!(
        r#"
        io.stdout:setvbuf("line")
        local pid = {}
        caim.defer(function()
            caim.stop(pid, {{ "TERM:0", "KILL:30" }})
            local f = io.open(caim.args.dir .. "/cleaned", "w"); f:write("yes"); f:close()
        end)
        caim.run{{ "true", output = caim.args.dir .. "/run.log", idle = 60 }}
        assert(caim.line(caim.args.dir .. "/alpha.log", "^ready$", pid))
        print("ready")
        caim.exit(pid)
        print("not reached")
        "#,
        blocked_child(&dir, "alpha")
    );
    let mut child = spawn(&mut script(&dir, &source));
    assert_eq!(stdout_line(child.stdout.as_mut().unwrap()), "ready\n");
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    let (status, output) = finish(child);
    assert_eq!(status, 128 + libc::SIGTERM, "{output}");
    assert!(!output.contains("not reached"), "{output}");
    assert_eq!(fs::read_to_string(dir.path("cleaned")).unwrap(), "yes");
}

#[test]
fn lines_iterates_the_matching_lines_with_their_captures() {
    let dir = TempDir::new();
    fs::write(dir.path("run.log"), "noise\nCASE ok alpha\nCASE bad beta\nmore noise\nCASE ok gamma\n").unwrap();
    let source = r#"
        local log = caim.args.dir .. "/run.log"
        for line, verdict, name in caim.lines(log, "^CASE (\\w+) (\\w+)$") do print(line, verdict, name) end
        local count = 0
        for _ in caim.lines(log, "^CASE") do count = count + 1 end
        print(count)
        for _ in caim.lines(caim.args.dir .. "/absent.log", ".") do print("never") end
        print((pcall(caim.lines, log, "(")))
    "#;
    let (status, output) = run(&mut script(&dir, source));
    let expected = "CASE ok alpha\tok\talpha\nCASE bad beta\tbad\tbeta\nCASE ok gamma\tok\tgamma\n3\nfalse\n";
    assert_eq!((status, output.as_str()), (0, expected));
}

#[test]
fn a_verdict_is_the_worst_of_its_cases_with_the_missing_ones_void() {
    let dir = TempDir::new();
    for (body, expected, last) in [
        ("v:pass('a'); v:pass('b')", 0, "VERDICT PASS cases=2 pass=2 fail=0 void=0"),
        ("v:pass('a'); v:fail('b')", 1, "VERDICT FAIL cases=2 pass=1 fail=1 void=0"),
        ("v:fail('a'); v:void('b'); v:pass('c')", 2, "VERDICT VOID cases=3 pass=1 fail=1 void=1"),
        ("v:pass('a'); v:expect{ 'a', 'missing' }", 2, "VERDICT VOID cases=2 pass=1 fail=0 void=1"),
        ("v:fail('a'); v:pass('a')", 1, "VERDICT FAIL cases=1 pass=0 fail=1 void=0"),
        ("v:pass('a'); v:record('MAYBE', 'b')", 2, "VERDICT VOID cases=2 pass=1 fail=0 void=1"),
        ("", 2, "VERDICT VOID cases=0 pass=0 fail=0 void=0"),
    ] {
        let source = format!("local v = caim.verdict()\n{body}\nreturn v:finish()");
        let (status, output) = run(&mut script(&dir, &source));
        assert_eq!(status, expected, "{body}: {output}");
        assert_eq!(output.lines().last(), Some(last), "{body}: {output}");
    }
    let source =
        "local v = caim.verdict()\nv:pass('beta'); v:fail('alpha'); v:expect{ 'gamma', 'alpha' }\nreturn v:finish()";
    let expected = "VERDICT VOID case=gamma\nVERDICT FAIL case=alpha\nVERDICT PASS case=beta\nVERDICT VOID cases=3 pass=1 fail=1 void=1\n";
    assert_eq!(run(&mut script(&dir, source)), (2, expected.into()));
}

#[test]
fn a_verdict_built_from_a_log() {
    let dir = TempDir::new();
    fs::write(dir.path("run.log"), "VERDICT PASS case=alpha\nVERDICT FAIL case=beta\nnoise\n").unwrap();
    let source = r#"
        local v = caim.verdict():expect{ "alpha", "beta", "gamma" }
        for _, verdict, name in caim.lines(caim.args.dir .. "/run.log", "^VERDICT (\\w+) case=(\\S+)") do
            v:record(verdict, name)
        end
        return v:finish()
    "#;
    let (status, output) = run(&mut script(&dir, source));
    assert_eq!(status, 2, "{output}");
    assert!(output.contains("VERDICT VOID case=gamma") && output.contains("VERDICT FAIL case=beta"), "{output}");
}

#[test]
fn hash_gives_shasum_lines_and_an_unreadable_path_gives_nil_and_why() {
    let dir = TempDir::new();
    fs::write(dir.path("alpha"), "abc").unwrap();
    fs::write(dir.path("beta"), "").unwrap();
    let source = r#"
        local dir = caim.args.dir
        io.write(caim.hash{ dir .. "/alpha", dir .. "/beta" })
        local text, why = caim.hash{ dir .. "/alpha", dir .. "/absent" }
        print(text, why:find("absent", 1, true) ~= nil)
        print(caim.hash{} == "")
    "#;
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let expected = format!("{abc}  {0}/alpha\n{empty}  {0}/beta\nnil\ttrue\ntrue\n", dir.0.display());
    assert_eq!(run(&mut script(&dir, source)), (0, expected));
}

#[test]
fn fresh_compares_files_with_the_stamps_made_of_them() {
    let dir = TempDir::new();
    fs::write(dir.path("alpha"), "abc").unwrap();
    fs::write(dir.path("beta"), "abc").unwrap();
    let source = r#"
        local dir = caim.args.dir
        local function stamp(name, paths)
            local file = assert(io.open(dir .. "/" .. name, "w")); file:write(caim.hash(paths)); file:close()
        end
        local function show(fresh, stale)
            local words = {}
            for _, entry in ipairs(stale) do
                words[#words + 1] = entry.list:match("[^/]+$") .. ":" .. (entry.path and entry.path:match("[^/]+$") or "-")
            end
            print(fresh, table.concat(words, " "))
        end
        stamp("one.stamp", { dir .. "/alpha" })
        stamp("two.stamp", { dir .. "/alpha", dir .. "/beta" })
        show(caim.fresh{ dir .. "/one.stamp", dir .. "/two.stamp" })
        local file = io.open(dir .. "/beta", "w"); file:write("changed"); file:close()
        show(caim.fresh{ dir .. "/one.stamp", dir .. "/two.stamp" })
        os.remove(dir .. "/alpha")
        show(caim.fresh{ dir .. "/one.stamp", dir .. "/two.stamp" })
        show(caim.fresh{ dir .. "/never.stamp" })
        local bad = io.open(dir .. "/bad.stamp", "w"); bad:write("not a stamp\n"); bad:close()
        print((pcall(caim.fresh, { dir .. "/bad.stamp" })))
    "#;
    let expected = "true\t\nfalse\ttwo.stamp:beta\nfalse\tone.stamp:alpha two.stamp:alpha two.stamp:beta\nfalse\tnever.stamp:-\nfalse\n";
    assert_eq!(run(&mut script(&dir, source)), (0, expected.into()));
}

#[test]
fn modules_are_found_next_to_the_script_and_in_the_projects_lua_directory() {
    let project = TempDir::new();
    fs::write(project.path("caim.toml"), "name = \"case\"\n").unwrap();
    fs::create_dir_all(project.path("lua/tools")).unwrap();
    fs::create_dir_all(project.path("tasks")).unwrap();
    fs::write(project.path("lua/shared.lua"), "return { name = 'shared' }").unwrap();
    fs::write(project.path("lua/tools/init.lua"), "return 'tools'").unwrap();
    fs::write(project.path("tasks/near.lua"), "return 'near'").unwrap();
    fs::write(project.path("outside.lua"), "return 'not on the path'").unwrap();
    let source = "print(require('shared').name, require('tools'), require('near'), (pcall(require, 'outside')))";
    fs::write(project.path("tasks/main.lua"), source).unwrap();
    let mut command = caim();
    command.args(["script", &project.text("tasks/main.lua")]).env("CAIM_ROOT", &project.0).current_dir(&project.0);
    assert_eq!(run(&mut command), (0, "shared\ttools\tnear\tfalse\n".into()));
}

#[test]
fn invoke_runs_another_task_with_arguments_and_returns_its_status() {
    let project = TempDir::new();
    let locks = TempDir::new();
    let tasks = r#"name = "case"
[tasks.greet]
run = ["sh", "-c", "echo greet $CAIM_ARG_WHO ${CAIM_ARG_LOUD:-quiet} ${CAIM_ARG_PARENT:-unset}; exit ${CAIM_ARG_CODE:-0}"]
[[tasks.greet.args]]
name = "who"
help = "whom"
default = "nobody"
[[tasks.greet.args]]
name = "loud"
help = "loudly"
flag = true
[[tasks.greet.args]]
name = "code"
help = "the status"
default = "0"
"#;
    fs::write(project.path("caim.toml"), tasks).unwrap();
    let source = r#"
        print(caim.invoke("greet", { who = "alpha", loud = true, quiet = false }))
        print(caim.invoke("greet", { code = 3 }))
        print((pcall(caim.invoke, "greet", { who = {} })))
        return caim.invoke("absent")
    "#;
    fs::write(project.path("main.lua"), source).unwrap();
    let mut command = caim();
    for (key, _) in std::env::vars() {
        if key.starts_with("CAIM_") {
            command.env_remove(key);
        }
    }
    command
        .args(["script", &project.text("main.lua")])
        .env("CAIM_ROOT", &project.0)
        .env("CAIM_LOCK_DIR", &locks.0)
        .env("CAIM_ARG_PARENT", "leaked")
        .current_dir(&project.0);
    let (status, output) = run(&mut command);
    assert!(output.contains("greet alpha 1 unset\n[greet] exit=0\n0\n"), "{output}");
    assert!(output.contains("greet nobody quiet unset\n[greet] exit=3\n3\n"), "{output}");
    assert!(output.contains("false\n"), "{output}");
    assert_eq!(status, 64, "an unknown task is caim's own 64: {output}");
}

#[test]
fn script_is_not_a_task_name() {
    let dir = TempDir::new();
    fs::write(dir.path("caim.toml"), "name = \"case\"\n[tasks.script]\nrun = \"true\"\n").unwrap();
    let (status, output) = run(caim().arg("list").env("CAIM_ROOT", &dir.0));
    assert_eq!(status, 78, "{output}");
}
