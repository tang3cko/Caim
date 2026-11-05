mod common;
use common::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::rc::Rc;

struct Project {
    dir: TempDir,
    locks: Rc<TempDir>,
}

impl Project {
    fn new() -> Self {
        Self::sharing_locks(Rc::new(TempDir::new()))
    }
    fn sharing_locks(locks: Rc<TempDir>) -> Self {
        Self::named("case", locks)
    }
    fn named(name: &str, locks: Rc<TempDir>) -> Self {
        let dir = TempDir::new();
        fs::write(dir.path("caim.toml"), format!("name = \"{name}\"\n")).unwrap();
        fs::create_dir_all(dir.path("scripts")).unwrap();
        Project { dir, locks }
    }
    fn executable(&self, relative: &str, script: &str) {
        let path = self.dir.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("#!/bin/bash\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn task(&self, group: &str, name: &str, toml: &str, script: &str) {
        self.executable(&format!("scripts/{name}"), script);
        let body: String = toml
            .lines()
            .map(|line| match line.strip_prefix("[[") {
                Some(rest) => format!("[[tasks.{name}.{rest}\n"),
                None => match line.strip_prefix('[') {
                    Some(rest) => format!("[tasks.{name}.{rest}\n"),
                    None => format!("{line}\n"),
                },
            })
            .collect();
        let (head, tables) = body.split_at(body.find("\n[").map_or(body.len(), |at| at + 1));
        self.written(&format!("\n[tasks.{name}]\ngroup = \"{group}\"\nrun = [\"scripts/{name}\"]\n{head}{tables}"));
    }
    fn written(&self, tasks: &str) {
        let path = self.dir.path("caim.toml");
        let name = fs::read_to_string(&path).unwrap();
        fs::write(path, format!("{name}{tasks}")).unwrap();
    }
    fn hook(&self, name: &str, script: &str) {
        self.executable(&format!("scripts/hook-{name}"), script);
        self.written(&format!("\n[hooks.{name}]\nrun = [\"scripts/hook-{name}\"]\n"));
    }
    fn x(&self, args: &[&str]) -> Command {
        isolated(&self.dir.0, &self.locks.0, args)
    }
    fn run(&self, args: &[&str]) -> (i32, String) {
        run(&mut self.x(args))
    }
    fn untouched(&self, extra: &[&str]) {
        let mut names: Vec<String> = fs::read_dir(&self.dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        names.sort();
        let mut expected: Vec<String> =
            ["caim.toml", "scripts"].iter().chain(extra).map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(names, expected, "caim wrote into the project");
    }
}

fn isolated(root: &Path, locks: &Path, args: &[&str]) -> Command {
    let mut command = caim();
    for (key, _) in std::env::vars() {
        if key.starts_with("CAIM_") {
            command.env_remove(key);
        }
    }
    command.args(args).env("CAIM_ROOT", root).env("CAIM_LOCK_DIR", locks).env("CAIM", env!("CARGO_BIN_EXE_caim"));
    command
}

fn split(command: &mut Command) -> (i32, String, String) {
    let Output { status, stdout, stderr } = command.stdin(Stdio::null()).output().unwrap();
    (status_of(status), String::from_utf8_lossy(&stdout).into(), String::from_utf8_lossy(&stderr).into())
}

fn last_line(output: &str) -> &str {
    output.lines().last().unwrap_or_default()
}

const SIMPLE: &str = "description = \"a task\"\n";

#[test]
fn list_shows_tasks_by_group() {
    let project = Project::new();
    project.task("build", "alpha", SIMPLE, "exit 0");
    project.task(
        "deploy",
        "beta",
        "description = \"b\"\nhuman = true\n[[args]]\nname = \"filter\"\nhelp = \"which\"\ndefault = \"all\"\n",
        "exit 0",
    );
    project.task("build", "internal", "description = \"i\"\nhidden = true\n", "exit 0");
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("build:\n  alpha"), "{output}");
    assert!(output.contains("deploy:\n  beta"), "{output}");
    assert!(output.contains("(a person runs it)"), "{output}");
    assert!(!output.contains("internal"), "{output}");
    let (status, help) = project.run(&["beta", "--help"]);
    assert_eq!(status, 0, "{help}");
    assert!(help.contains("default all"), "{help}");
    assert!(help.contains("run by a person, not an agent"), "{help}");
    assert_eq!(project.run(&["list", "--json"]).0, 64);
    project.untouched(&[]);
}

#[test]
fn bad_usage_is_64_before_anything_runs() {
    let project = Project::new();
    let toml = format!(
        "{SIMPLE}[[args]]\nname = \"step\"\nhelp = \"which step\"\nchoices = [\"all\", \"gather\"]\ndefault = \"all\"\n\
         [[args]]\nname = \"target\"\nhelp = \"what\"\nrequired = true\n[[args]]\nname = \"fast\"\nhelp = \"quick\"\nflag = true\n"
    );
    project.task("check", "picky", &toml, "touch \"$RUNNER_TEST_MARK\"");
    let mark = project.dir.path("ran");
    for args in [
        vec!["picky", "--bogus"],
        vec!["picky", "--target", "a", "--bogus"],
        vec!["picky", "positional", "--target", "a"],
        vec!["picky"],
        vec!["picky", "--target", "a", "--step", "compile"],
        vec!["picky", "--target", "a", "--fast=1"],
        vec!["picky", "--target"],
        vec!["picky", "--target", "a", "--target", "b"],
        vec!["nonexistent"],
    ] {
        let (status, output) = run(project.x(&args).env("RUNNER_TEST_MARK", &mark));
        assert_eq!(status, 64, "{args:?}: {output}");
        assert!(!output.contains("] exit="), "{args:?}: {output}");
    }
    assert!(!mark.exists(), "a task ran on bad usage");
    project.untouched(&[]);
    let (status, output) = project.run(&["picky", "--help"]);
    assert_eq!(status, 0);
    assert!(output.contains("usage: caim picky [--step <value>] --target <value> [--fast]"), "{output}");
    assert!(output.contains("one of all, gather; default all"), "{output}");
}

#[test]
fn a_broken_declaration_stops_everything_as_setup() {
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        ("check", "typo", "description = \"t\"\ngaurd = true\n", "unknown field `gaurd`"),
        ("check", "layered", "description = \"t\"\nlayer = \"check\"\n", "unknown field `layer`"),
        ("Bad_Group", "grouped", SIMPLE, "group \"Bad_Group\" is not lowercase letters"),
        ("check", "stop", SIMPLE, "not a runner command"),
        ("check", "list", SIMPLE, "not a runner command"),
        ("check", "hooked", "description = \"t\"\nlock = \"absent\"\n", "no hook absent in [hooks]"),
        ("check", "needy", "description = \"t\"\nrequires = [\"ghost\"]\n", "requires unknown task ghost"),
        ("check", "loop", "description = \"t\"\nrequires = [\"loop\"]\n", "requirement cycle"),
        (
            "check",
            "helpful",
            "description = \"t\"\n[[args]]\nname = \"help\"\nhelp = \"h\"\n",
            "not a valid argument name",
        ),
        (
            "check",
            "choosy",
            "description = \"t\"\n[[args]]\nname = \"a\"\nhelp = \"h\"\nchoices = [\"x\"]\ndefault = \"y\"\n",
            "default that is not one of its choices",
        ),
    ];
    for (group, name, toml, expected) in cases {
        let project = Project::new();
        project.task("check", "fine", SIMPLE, "exit 0");
        project.task(group, name, toml, "exit 0");
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{name}: {output}");
        assert!(output.contains(expected), "{name}: {output}");
        assert_eq!(project.run(&["fine"]).0, 78, "{name}");
    }
    let project = Project::new();
    project.task("check", "typo", "description = \"t\"\ngaurd = true\n", "exit 0");
    let (_, output) = project.run(&["list"]);
    assert!(output.contains("line 7"), "the error names the line: {output}");
    let project = Project::new();
    project.task("check", "plain", SIMPLE, "exit 0");
    fs::set_permissions(project.dir.path("scripts/plain"), fs::Permissions::from_mode(0o644)).unwrap();
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 0, "{output}");
    let (status, output) = project.run(&["plain"]);
    assert_eq!(status, 126, "{output}");
    assert!(output.contains("plain did not start; scripts/plain is not executable"), "{output}");
    assert_eq!(last_line(&output), "[plain] exit=126 (did not start)", "{output}");
    let project = Project::new();
    project.task("check", "plain", SIMPLE, "exit 0");
    project.written("\n[hooks.gone]\nrun = [\"scripts/gone\"]\n");
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 78);
    assert!(output.contains("[hooks.gone]: scripts/gone is missing or not executable"), "{output}");
}

#[test]
fn several_tasks_run_in_order_and_stop_at_the_first_that_does_not_pass() {
    let project = Project::new();
    let say = "description = \"t\"\n[[args]]\nname = \"word\"\nhelp = \"w\"\ndefault = \"none\"\n";
    project.task("check", "one", say, "echo \"one $CAIM_ARG_WORD\" >> \"$ORDER\"");
    project.task("check", "two", say, "echo \"two $CAIM_ARG_WORD\" >> \"$ORDER\"");
    project.task("check", "fails", SIMPLE, "echo fails >> \"$ORDER\"; exit 5");
    project.task("check", "top", "description = \"t\"\nrequires = [\"one\"]\n", "echo top >> \"$ORDER\"");
    let order = project.dir.path("order");
    let ran = |args: &[&str]| {
        let _ = fs::remove_file(&order);
        let (status, stdout, stderr) = split(project.x(args).env("ORDER", &order));
        (status, stdout, stderr, fs::read_to_string(&order).unwrap_or_default())
    };

    let (status, stdout, stderr, order_text) = ran(&["one", "--word", "a", "two", "--word=b"]);
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(order_text, "one a\ntwo b\n");
    assert_eq!(stdout, "[one] exit=0\n[two] exit=0\n");

    let (status, _, _, order_text) = ran(&["one", "--word", "two"]);
    assert_eq!(status, 0);
    assert_eq!(order_text, "one two\n", "a value that reads like a task is the option's value");

    let (status, stdout, _, order_text) = ran(&["one", "fails", "two"]);
    assert_eq!(status, 5);
    assert_eq!(order_text, "one none\nfails\n", "nothing after the first failure runs");
    assert_eq!(last_line(&stdout), "[fails] exit=5");

    let (status, _, stderr, order_text) = ran(&["one", "two", "--bogus", "x"]);
    assert_eq!(status, 64, "{stderr}");
    assert_eq!(order_text, "", "a bad argument to a later task stops the line before anything runs");
    assert!(stderr.contains("two: unknown option --bogus"), "{stderr}");

    let (status, _, _, order_text) = ran(&["top", "one", "one"]);
    assert_eq!(status, 0);
    assert_eq!(order_text, "one none\ntop\n", "a task already run, as a requirement or before, runs once");

    let (status, stdout, _, _) = ran(&["one", "two"]);
    assert_eq!(status, 0);
    let verdicts: Vec<&str> = stdout.lines().filter(|line| line.starts_with('[')).collect();
    assert_eq!(verdicts, vec!["[one] exit=0", "[two] exit=0"]);

    let (status, stdout, _, order_text) = ran(&["one", "two", "--help"]);
    assert_eq!(status, 0);
    assert!(stdout.contains("usage: caim one") && stdout.contains("usage: caim two"), "{stdout}");
    assert_eq!(order_text, "");

    for args in [vec!["one", "ghost"], vec!["--word", "a", "one"]] {
        let (status, _, stderr, order_text) = ran(&args);
        assert_eq!(status, 64, "{args:?}: {stderr}");
        assert_eq!(order_text, "", "{args:?}");
    }
}

#[test]
fn a_task_written_in_caim_toml_runs_like_any_other() {
    let project = Project::new();
    project.written(
        "\n[tasks.greet]\ndescription = \"says hello\"\nrun = 'echo \"hello $CAIM_ARG_WHO from $PWD\" | tr a-z A-Z'\n\
         [[tasks.greet.args]]\nname = \"who\"\nhelp = \"whom\"\ndefault = \"world\"\n\n\
         [tasks.literal]\ndescription = \"no shell\"\ngroup = \"tools\"\nrun = [\"echo\", \"$HOME\", \"a  b\"]\n\n\
         [tasks.both]\ndescription = \"after greet\"\nrequires = [\"greet\"]\nrun = \"echo both\"\n\n\
         [tasks.bare]\nrun = \"echo bare\"\n",
    );
    project.task("check", "scripted", SIMPLE, "echo scripted");
    let (status, stdout, stderr) = split(&mut project.x(&["greet", "--who", "you"]));
    assert_eq!(status, 0, "{stdout}{stderr}");
    let root = project.dir.0.display().to_string().to_uppercase();
    assert_eq!(stdout, format!("HELLO YOU FROM {root}\n[greet] exit=0\n"));
    let (status, stdout, _) = split(&mut project.x(&["literal"]));
    assert_eq!(status, 0);
    assert_eq!(stdout.lines().next(), Some("$HOME a  b"));
    let (status, output) = project.run(&["both"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("[greet] exit=0"), "{output}");
    assert_eq!(project.run(&["greet", "--bogus", "x"]).0, 64);

    let (status, stdout, _) = split(&mut project.x(&["list"]));
    assert_eq!(status, 0);
    assert!(stdout.starts_with("  bare                   \n  both "), "ungrouped tasks come first: {stdout}");
    assert!(stdout.contains("check:\n  scripted"), "{stdout}");
    assert!(stdout.contains("tools:\n  literal"), "{stdout}");
    let (_, help) = project.run(&["greet", "--help"]);
    assert!(help.contains("declared in caim.toml [tasks.greet]"), "{help}");
    let (status, help) = project.run(&["bare", "--help"]);
    assert_eq!(status, 0, "{help}");
    assert!(help.starts_with("bare\n\nusage: caim bare"), "a task needs no description: {help}");

    for (tasks, expected) in [
        ("\n[tasks.empty]\ndescription = \"t\"\nrun = []\n", "run is an empty list"),
        ("\n[tasks.silent]\ndescription = \"t\"\n", "missing field `run`"),
        ("\n[tasks.scripted]\ndescription = \"t\"\nrun = \"echo\"\n", "duplicate key"),
    ] {
        let other = Project::new();
        other.task("check", "scripted", SIMPLE, "echo scripted");
        other.written(tasks);
        let (status, output) = other.run(&["list"]);
        assert_eq!(status, 78, "{tasks}: {output}");
        assert!(output.contains(expected), "{tasks}: {output}");
    }
}

#[test]
fn a_passing_run_streams_its_output_and_ends_in_the_verdict() {
    let project = Project::new();
    let toml = format!(
        "{SIMPLE}[[args]]\nname = \"spec\"\nhelp = \"s\"\n[[args]]\nname = \"dry-run\"\nhelp = \"d\"\nflag = true\n"
    );
    project.task(
        "check",
        "ok",
        &toml,
        "echo \"spec=$CAIM_ARG_SPEC dry=$CAIM_ARG_DRY_RUN cwd=$PWD run=${GUARD_RUN_DIR-unset}\"\necho 'to stderr' >&2\nfor i in 1 2 3; do echo \"line $i\"; done",
    );
    let (status, stdout, stderr) = split(&mut project.x(&["ok", "--spec", "[tag] name", "--dry-run"]));
    assert_eq!(status, 0, "{stdout}{stderr}");
    let expected = format!(
        "spec=[tag] name dry=1 cwd={} run=unset\nto stderr\nline 1\nline 2\nline 3\n[ok] exit=0\n",
        project.dir.0.display()
    );
    assert_eq!(stdout, expected, "stderr: {stderr}");
    project.untouched(&[]);
}

#[test]
fn a_tasks_exit_status_is_caims() {
    let project = Project::new();
    project.task("check", "ends", SIMPLE, "echo \"ended $WANT\"; exit $WANT");
    for code in [1, 2, 3, 4, 5, 64, 124, 125, 126, 127] {
        let (status, stdout, stderr) = split(project.x(&["ends"]).env("WANT", code.to_string()));
        assert_eq!(status, code, "{stdout}{stderr}");
        assert_eq!(last_line(&stdout), format!("[ends] exit={code}"), "{stdout}");
    }
}

#[test]
fn a_command_that_cannot_start_exits_as_a_shell_says() {
    let project = Project::new();
    project.written("\n[tasks.nothing]\nrun = [\"no-such-command-anywhere\"]\n");
    let (status, output) = project.run(&["nothing"]);
    assert_eq!(status, 127, "{output}");
    assert_eq!(last_line(&output), "[nothing] exit=127 (did not start)", "{output}");
}

#[test]
fn required_tasks_run_first_and_stop_the_run_when_they_fail() {
    let project = Project::new();
    project.task("check", "base", SIMPLE, "echo base >> \"$ORDER\"; exit ${BASE_STATUS:-0}");
    project.task("check", "top", "description = \"t\"\nrequires = [\"base\"]\n", "echo top >> \"$ORDER\"");
    let order = project.dir.path("order");
    let (status, output) = run(project.x(&["top"]).env("ORDER", &order));
    assert_eq!(status, 0, "{output}");
    assert_eq!(fs::read_to_string(&order).unwrap(), "base\ntop\n");
    assert!(output.contains("[base] exit=0"), "{output}");
    fs::remove_file(&order).unwrap();
    let (status, output) = run(project.x(&["top"]).env("ORDER", &order).env("BASE_STATUS", "1"));
    assert_eq!(status, 1, "{output}");
    assert_eq!(fs::read_to_string(&order).unwrap(), "base\n");
    assert_eq!(last_line(&output), "[top] exit=1 (required task base did not pass)", "{output}");
}

#[test]
fn a_lock_hook_refuses_before_the_run_starts() {
    let project = Project::new();
    project.hook("editor-lock", "echo \"BLOCKED: $CAIM_TASK did not start\"; exit 4");
    project.task("check", "locked", "description = \"t\"\nlock = \"editor-lock\"\n", "touch \"$MARK\"");
    let mark = project.dir.path("ran");
    let (status, stdout, stderr) = split(project.x(&["locked"]).env("MARK", &mark));
    assert_eq!(status, 4, "{stdout}{stderr}");
    assert!(stdout.contains("BLOCKED: locked did not start"), "{stdout}");
    assert_eq!(last_line(&stdout), "[locked] exit=4 (editor-lock hook)", "{stdout}");
    assert!(!mark.exists());
    project.untouched(&[]);
}

#[test]
fn after_hooks_see_the_status_and_may_change_it() {
    let project = Project::new();
    project.hook(
        "sweep",
        "test -f \"$CAIM_STARTED\" || exit 9; echo \"swept $CAIM_TASK status=$CAIM_STATUS\"; exit ${SWEEP_STATUS:-0}",
    );
    project.task("check", "swept", "description = \"t\"\nafter = [\"sweep\"]\n", "echo 'done'");
    let (status, output) = project.run(&["swept"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("swept swept status=0"), "{output}");
    let (status, output) = run(project.x(&["swept"]).env("SWEEP_STATUS", "5"));
    assert_eq!(status, 5, "{output}");
    assert_eq!(last_line(&output), "[swept] exit=5", "{output}");
    project.untouched(&[]);
}

#[test]
fn a_silent_run_is_stopped_as_stalled() {
    let project = Project::new();
    project.task("check", "silent", SIMPLE, "echo started; exec tail -f /dev/null");
    let (status, stdout, stderr) = split(project.x(&["silent"]).env("CAIM_STALL_SECONDS", "1"));
    assert_eq!(status, 124, "{stdout}{stderr}");
    assert!(stderr.contains("made no progress for 1 s"), "{stderr}");
    assert!(stderr.contains("[stop] TERM to 1 process(es)"), "{stderr}");
    assert_eq!(stdout, "started\n[silent] exit=124 (stalled)\n");
}

#[test]
fn an_interrupted_run_is_void() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        let project = Project::new();
        project.task("check", "long", SIMPLE, "echo ready > \"$READY\"; exec tail -f /dev/null");
        let ready = project.dir.path("ready");
        fs::write(&ready, "").unwrap();
        let child = spawn(project.x(&["long"]).env("READY", &ready));
        written(&ready);
        unsafe { libc::kill(child.id() as i32, signal) };
        let (status, output) = finish(child);
        assert_eq!(status, 128 + signal, "signal {signal}: {output}");
        assert!(output.contains("[stop] TERM to 1 process(es)"), "signal {signal}: {output}");
        assert!(output.contains("long was interrupted"), "signal {signal}: {output}");
        assert_eq!(
            last_line(&output),
            format!("[long] exit={} (interrupted)", 128 + signal),
            "signal {signal}: {output}"
        );
    }
}

#[test]
fn a_signal_ignored_at_start_stays_ignored_between_runs() {
    let project = Project::new();
    let ready = project.dir.path("ready");
    let release = project.dir.path("release");
    fs::write(&ready, "").unwrap();
    fs::write(&release, "").unwrap();
    project.hook("hold", "echo ready > \"$READY\"; \"$CAIM\" written \"$RELEASE\" > /dev/null");
    project.task("check", "short", "description = \"s\"\nafter = [\"hold\"]\n", "echo done");
    let mut command = project.x(&["short"]);
    command.env("READY", &ready).env("RELEASE", &release);
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = spawn(&mut command);
    written(&ready);
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    fs::write(&release, "go\n").unwrap();
    let (status, output) = finish(child);
    assert_eq!(status, 0, "{output}");
    assert_eq!(last_line(&output), "[short] exit=0", "{output}");
}

#[test]
fn a_declaration_the_watch_cannot_follow_stops_the_run() {
    let project = Project::new();
    project.task("check", "declares", SIMPLE, "echo \"tree /\" >> \"$CAIM_DECLARED\"; exec tail -f /dev/null");
    let (status, output) = project.run(&["declares"]);
    assert_eq!(status, 125, "{output}");
    assert!(output.contains("its progress could not be watched (unknown watch declaration: tree /)"), "{output}");
    assert!(!output.contains("no progress for"), "{output}");
    assert_eq!(last_line(&output), "[declares] exit=125 (unavailable)", "{output}");
}

#[test]
fn the_files_a_task_talks_back_through_are_gone_after_the_run() {
    let project = Project::new();
    project.task(
        "check",
        "talks",
        SIMPLE,
        "echo \"$CAIM_DECLARED\"; echo \"$CAIM_LAUNCH_REGISTRY\"; echo \"$CAIM_WATCHERS\"",
    );
    let (status, stdout, stderr) = split(&mut project.x(&["talks"]));
    assert_eq!(status, 0, "{stdout}{stderr}");
    let paths: Vec<&str> = stdout.lines().take(3).collect();
    for path in &paths {
        assert!(!path.is_empty() && !path.starts_with(&*project.dir.0.to_string_lossy()), "{stdout}");
        assert!(!Path::new(path).exists(), "{path} outlived the run");
        assert!(!Path::new(path).parent().unwrap().exists(), "the directory of {path} outlived the run");
    }
    project.untouched(&[]);
}

#[test]
fn a_leftover_holding_the_output_does_not_hold_the_run() {
    let project = Project::new();
    project.task("check", "forks", SIMPLE, "tail -f /dev/null &\necho \"left $!\"\nexit 0");
    let (status, stdout, stderr) = split(&mut project.x(&["forks"]));
    let left: i32 = stdout.lines().next().and_then(|line| line.strip_prefix("left ")).unwrap().parse().unwrap();
    unsafe { libc::kill(left, libc::SIGKILL) };
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(last_line(&stdout), "[forks] exit=0", "{stdout}");
}

#[test]
fn a_reader_that_leaves_early_does_not_end_the_supervision() {
    let project = Project::new();
    project.task(
        "check",
        "chatty",
        SIMPLE,
        "echo first\n\"$CAIM\" written \"$GO\" 180 >&2\nfor i in $(seq 1 2000); do echo \"line $i\"; done\nexit 7",
    );
    let go = project.dir.path("go");
    fs::write(&go, "").unwrap();
    let mut child = project
        .x(&["chatty"])
        .env("GO", &go)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    assert_eq!(stdout_line(&mut stdout), "first\n");
    drop(stdout);
    append(&go, b"go\n");
    let status = status_of(child.wait().unwrap());
    assert_eq!(status, 7);
}

#[test]
fn dry_run_shows_the_plan_and_runs_nothing() {
    let project = Project::new();
    project.task("check", "base", "description = \"b\"\nconcurrency = \"editor\"\n", "touch \"$MARK\"");
    project.task(
        "check",
        "top",
        "description = \"t\"\nrequires = [\"base\"]\n[[args]]\nname = \"n\"\nhelp = \"h\"\ndefault = \"1\"\n",
        "touch \"$MARK\"",
    );
    let scratch = TempDir::new();
    let mark = scratch.path("ran");
    let (status, stdout, stderr) = split(project.x(&["--dry-run", "top", "--n", "2", "base"]).env("MARK", &mark));
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert!(!mark.exists(), "a dry run ran a task");
    let names: Vec<&str> = stdout.lines().filter(|line| !line.starts_with(' ')).collect();
    assert_eq!(names, ["base", "top"], "requirements first, and no task twice: {stdout}");
    assert!(stdout.contains("  run: scripts/top\n"), "{stdout}");
    assert!(stdout.contains("  args: CAIM_ARG_N=\"2\"\n"), "{stdout}");
    assert!(stdout.contains("  holds: editor (alone, per project)\n"), "{stdout}");
    assert_eq!(project.run(&["--dry-run", "top", "--bogus"]).0, 64, "arguments are still checked");
    assert_eq!(project.run(&["top", "--dry-run"]).0, 64, "after a task, --dry-run is the task's own option");
}

#[test]
fn parallel_requirements_run_at_once_and_their_own_requirements_once() {
    let project = Project::new();
    let scratch = TempDir::new();
    let go = scratch.path("go");
    let count = scratch.path("count");
    project.task("check", "base", SIMPLE, "echo base >> \"$COUNT\"");
    // waiter finishes only once writer has written: run one after the other, it would wait out its guard.
    project.task(
        "check",
        "waiter",
        "description = \"w\"\nrequires = [\"base\"]\n",
        "\"$CAIM\" written \"$GO\" 30 > /dev/null && echo waited",
    );
    project.task("check", "writer", "description = \"w\"\nrequires = [\"base\"]\n", "echo go > \"$GO\"; echo wrote");
    project.task(
        "check",
        "top",
        "description = \"t\"\nrequires = [\"waiter\", \"writer\"]\nparallel = true\n",
        "echo top",
    );
    fs::write(&go, "").unwrap();
    let (status, stdout, stderr) = split(project.x(&["top"]).env("GO", &go).env("COUNT", &count));
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(fs::read_to_string(&count).unwrap(), "base\n", "a shared requirement runs once");
    assert!(stdout.contains("[waiter] waited\n"), "{stdout}");
    assert!(stdout.contains("[writer] wrote\n"), "{stdout}");
    assert!(stdout.contains("[waiter] exit=0\n") && !stdout.contains("[waiter] [waiter]"), "{stdout}");
    assert!(stdout.ends_with("top\n[top] exit=0\n"), "{stdout}");

    project.task("check", "failing", SIMPLE, "exit 3");
    project.task(
        "check",
        "fan",
        "description = \"f\"\nrequires = [\"failing\", \"writer\"]\nparallel = true\n",
        "echo fan",
    );
    let (status, stdout, _) = split(project.x(&["fan"]).env("GO", &go).env("COUNT", &count));
    assert_eq!(status, 3, "{stdout}");
    assert_eq!(last_line(&stdout), "[fan] exit=3 (required task failing did not pass)", "{stdout}");

    project.task("check", "shell", "description = \"s\"\nguard = false\n", "true");
    project.task("check", "both", "description = \"b\"\nrequires = [\"shell\"]\nparallel = true\n", "true");
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 78, "{output}");
    assert!(output.contains("both: runs shell in parallel, but it has guard = false"), "{output}");
}

const BUILD: &str = "description = \"b\"\n[[args]]\nname = \"configuration\"\nhelp = \"h\"\ndefault = \"Release\"\n\
    choices = [\"Debug\", \"Release\"]\n[[args]]\nname = \"clean\"\nhelp = \"h\"\nflag = true\n";
const BUILD_RUN: &str = "echo \"build $CAIM_ARG_CONFIGURATION clean=$CAIM_ARG_CLEAN\" >> \"$ORDER\"";
const PASSES: &str = "description = \"i\"\n\
    requires = [{ task = \"build\", args = { configuration = { arg = \"configuration\" } } }]\n\
    [[args]]\nname = \"configuration\"\nhelp = \"h\"\ndefault = \"Release\"\n";

/// build, perfcheck requiring a Debug build, and install passing its own --configuration to build.
fn building() -> Project {
    let project = Project::new();
    project.task("check", "build", BUILD, BUILD_RUN);
    project.task(
        "check",
        "perfcheck",
        "description = \"p\"\nrequires = [{ task = \"build\", args = { configuration = \"Debug\", clean = true } }]\n",
        "echo perfcheck >> \"$ORDER\"",
    );
    project.task("check", "install", PASSES, "echo install >> \"$ORDER\"");
    project
}

fn ordered(project: &Project, args: &[&str]) -> (i32, String, String) {
    let scratch = TempDir::new();
    let order = scratch.path("order");
    let (status, output) = run(project.x(args).env("ORDER", &order));
    (status, fs::read_to_string(&order).unwrap_or_default(), output)
}

#[test]
fn requirements_take_fixed_arguments_and_the_requiring_tasks_own() {
    let project = building();
    let (status, order, output) = ordered(&project, &["perfcheck"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Debug clean=1\nperfcheck\n");
    let (status, order, output) = ordered(&project, &["install", "--configuration", "Debug"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Debug clean=\ninstall\n");
    let (status, order, output) = ordered(&project, &["install"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Release clean=\ninstall\n");

    let (status, order, output) = ordered(&project, &["install", "--configuration", "Fast"]);
    assert_eq!(status, 64, "{output}");
    assert_eq!(order, "", "a passed value is checked before anything runs");
    assert!(output.contains("install: build --configuration must be one of Debug, Release, not \"Fast\""), "{output}");

    let (_, help) = project.run(&["perfcheck", "--help"]);
    assert!(help.contains("runs build --clean --configuration Debug first"), "{help}");
    let (_, help) = project.run(&["install", "--help"]);
    assert!(help.contains("runs build --configuration <configuration> first"), "{help}");
}

#[test]
fn a_task_runs_once_for_each_set_of_arguments() {
    let project = building();
    let (status, order, output) = ordered(&project, &["build", "install"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Release clean=\ninstall\n", "the same arguments run once");
    let (status, order, output) = ordered(&project, &["build", "perfcheck"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Release clean=\nbuild Debug clean=1\nperfcheck\n", "other arguments run again");
    let (status, order, output) = ordered(&project, &["build", "--configuration", "Debug", "--clean", "perfcheck"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(order, "build Debug clean=1\nperfcheck\n");

    let (status, stdout, _) = split(&mut project.x(&["--dry-run", "build", "perfcheck"]));
    assert_eq!(status, 0, "{stdout}");
    let names: Vec<&str> = stdout.lines().filter(|line| !line.starts_with(' ')).collect();
    assert_eq!(names, ["build", "build", "perfcheck"], "{stdout}");
    assert!(stdout.contains("  args: CAIM_ARG_CLEAN=\"1\" CAIM_ARG_CONFIGURATION=\"Debug\"\n"), "{stdout}");
}

#[test]
fn parallel_requirements_get_their_arguments() {
    let project = Project::new();
    project.task("check", "build", BUILD, BUILD_RUN);
    project.task("check", "lint", SIMPLE, "echo lint >> \"$ORDER\"");
    project.task(
        "check",
        "fan",
        "description = \"f\"\nparallel = true\n\
         requires = [{ task = \"build\", args = { configuration = \"Debug\" } }, \"lint\"]\n",
        "echo fan >> \"$ORDER\"",
    );
    let (status, order, output) = ordered(&project, &["fan"]);
    assert_eq!(status, 0, "{output}");
    let mut lines: Vec<&str> = order.lines().collect();
    assert_eq!(lines.pop(), Some("fan"), "{order}");
    lines.sort();
    assert_eq!(lines, ["build Debug clean=", "lint"], "{order}");
    assert!(output.contains("[build] exit=0") && output.contains("[lint] exit=0"), "{output}");
}

#[test]
fn a_bad_requirement_is_setup() {
    let flag_arg = "\n[[args]]\nname = \"fast\"\nhelp = \"h\"\nflag = true\n";
    for (requirement, extra, expected) in [
        ("{ task = \"build\", args = { nope = \"x\" } }", "", "top: requires build with unknown argument --nope"),
        (
            "{ task = \"build\", args = { configuration = \"Fast\" } }",
            "",
            "top: requires build with --configuration Fast, not one of Debug, Release",
        ),
        (
            "{ task = \"build\", args = { clean = \"yes\" } }",
            "",
            "top: requires build with --clean, a flag: give true or false",
        ),
        (
            "{ task = \"build\", args = { configuration = true } }",
            "",
            "top: requires build with --configuration, which takes a value, not true or false",
        ),
        (
            "{ task = \"build\", args = { configuration = { arg = \"missing\" } } }",
            "",
            "top: passes --missing to build, but top has no such argument",
        ),
        (
            "{ task = \"build\", args = { configuration = { arg = \"fast\" } } }",
            flag_arg,
            "top: passes --fast to build's --configuration; one is a flag and the other is not",
        ),
        ("{ task = \"build\", argz = {} }", "", "unknown field `argz`"),
        ("{ task = \"nothing\" }", "", "top: requires unknown task nothing"),
    ] {
        let project = Project::new();
        project.task("check", "build", BUILD, BUILD_RUN);
        project.task("check", "top", &format!("description = \"t\"\nrequires = [{requirement}]\n{extra}"), "true");
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{requirement}: {output}");
        assert!(output.contains(expected), "{requirement}: {output}");
    }

    let project = Project::new();
    project.task(
        "check",
        "deploy",
        "description = \"d\"\n[[args]]\nname = \"to\"\nhelp = \"h\"\nrequired = true\n",
        "true",
    );
    project.task("check", "bare", "description = \"b\"\nrequires = [\"deploy\"]\n", "true");
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 78, "{output}");
    assert!(output.contains("bare: requires deploy without its required --to"), "{output}");
    let project = Project::new();
    project.task(
        "check",
        "deploy",
        "description = \"d\"\n[[args]]\nname = \"to\"\nhelp = \"h\"\nrequired = true\n",
        "true",
    );
    project.task(
        "check",
        "given",
        "description = \"g\"\nrequires = [{ task = \"deploy\", args = { to = \"staging\" } }]\n",
        "true",
    );
    let (status, output) = project.run(&["given"]);
    assert_eq!(status, 0, "a required argument can be given: {output}");
}

#[test]
fn the_output_is_the_tasks_then_one_exit_line_each() {
    let project = Project::new();
    project.task("check", "base", SIMPLE, "echo 'from base'");
    project.task(
        "check",
        "top",
        "description = \"t\"\nrequires = [\"base\"]\n[[args]]\nname = \"n\"\nhelp = \"h\"\ndefault = \"1\"\n",
        "printf 'one\\ntwo \"quoted\"\\nno newline'",
    );
    let (status, stdout, stderr) = split(&mut project.x(&["top", "--n", "2"]));
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(stdout, "from base\n[base] exit=0\none\ntwo \"quoted\"\nno newline\n[top] exit=0\n");

    project.task("check", "silent", SIMPLE, "exec tail -f /dev/null");
    let (status, stdout, stderr) = split(project.x(&["silent"]).env("CAIM_STALL_SECONDS", "1"));
    assert_eq!(status, 124);
    assert_eq!(last_line(&stdout), "[silent] exit=124 (stalled)");
    assert!(stderr.contains("[stop] TERM"), "{stderr}");

    project.hook("refuse", "echo 'held'; exit 4");
    project.task("check", "held", "description = \"t\"\nlock = \"refuse\"\n", "exit 0");
    let (status, stdout, _) = split(&mut project.x(&["held"]));
    assert_eq!(status, 4);
    assert_eq!(stdout, "held\n[held] exit=4 (refuse hook)\n");
    assert_eq!(project.run(&["held", "--json"]).0, 64);
}

#[test]
fn an_unguarded_task_keeps_the_terminal() {
    let project = Project::new();
    project.task("dev", "plain", "description = \"t\"\nguard = false\n", "echo \"to the terminal\"");
    let (status, stdout, stderr) = split(&mut project.x(&["plain"]));
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(stdout, "to the terminal\n[plain] exit=0\n");
}

const HOLDS: &str =
    "echo $$ > \"$READY\"\n\"$CAIM\" written \"$GO\" 180 > /dev/null\necho \"$CAIM_TASK-end\" >> \"$ORDER\"";

struct Holder {
    child: Child,
    go: PathBuf,
    ready: PathBuf,
}

impl Holder {
    fn start(mut command: Command, label: &str, scratch: &Path) -> Self {
        let (ready, go) = (scratch.join(format!("{label}.ready")), scratch.join(format!("{label}.go")));
        fs::write(&ready, "").unwrap();
        fs::write(&go, "").unwrap();
        let child = spawn(command.env("READY", &ready).env("GO", &go).env("ORDER", scratch.join("order")));
        written(&ready);
        Holder { child, go, ready }
    }
    fn release(self) -> (i32, String) {
        append(&self.go, b"go\n");
        finish(self.child)
    }
}

fn stderr_line(stderr: &mut std::process::ChildStderr) -> String {
    use std::io::Read;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while stderr.read(&mut byte).unwrap() == 1 {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(line).unwrap()
}

const REFUSING: &str = "[concurrency]\ngroup = \"editor\"\non_busy = \"refuse\"\n";

#[test]
fn a_busy_group_refuses_a_run_that_asks_to_be_refused() {
    let project = Project::new();
    let scratch = TempDir::new();
    project.task("check", "first", "description = \"t\"\nconcurrency = \"editor\"\n", HOLDS);
    project.task("check", "second", &format!("description = \"t\"\n{REFUSING}"), "touch \"$MARK\"");
    project.task(
        "check",
        "other",
        "description = \"t\"\n[concurrency]\ngroup = \"assets\"\non_busy = \"refuse\"\n",
        "echo other",
    );
    let holder = Holder::start(project.x(&["first"]), "first", &scratch.0);
    let mark = scratch.path("ran");
    let (status, stdout, stderr) = split(project.x(&["second"]).env("MARK", &mark));
    assert_eq!(status, 75, "{stdout}{stderr}");
    let checkout = project.dir.0.display().to_string();
    assert!(stderr.contains("editor is held by first (pid "), "{stderr}");
    assert!(stderr.contains(&format!("in {checkout}")), "{stderr}");
    assert_eq!(last_line(&stdout), "[second] exit=75 (busy: editor)", "{stdout}");
    assert!(!mark.exists());
    let (status, output) = project.run(&["other"]);
    assert_eq!(status, 0, "another group is not held: {output}");
    let (status, output) = holder.release();
    assert_eq!(status, 0, "{output}");
    let (status, output) = run(project.x(&["second"]).env("MARK", &mark));
    assert_eq!(status, 0, "the group is free once the holder ends: {output}");
    project.untouched(&[]);
}

#[test]
fn a_busy_group_holds_a_waiting_run_until_it_is_free() {
    let project = Project::new();
    let scratch = TempDir::new();
    project.task("check", "first", "description = \"t\"\nconcurrency = \"editor\"\n", HOLDS);
    project.task("check", "later", "description = \"t\"\nconcurrency = \"editor\"\n", "echo later >> \"$ORDER\"");
    let holder = Holder::start(project.x(&["first"]), "first", &scratch.0);
    let mut waiting = project
        .x(&["later"])
        .env("ORDER", scratch.path("order"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let line = stderr_line(waiting.stderr.as_mut().unwrap());
    assert!(line.contains("later waits: editor is held by first (pid "), "{line}");
    let (status, output) = holder.release();
    assert_eq!(status, 0, "{output}");
    let (status, output) = finish(waiting);
    assert_eq!(status, 0, "{output}");
    assert_eq!(fs::read_to_string(scratch.path("order")).unwrap(), "first-end\nlater\n");
}

#[test]
fn a_holder_killed_before_it_signs_frees_a_waiting_run() {
    let project = Project::new();
    project.task("check", "later", "description = \"t\"\nconcurrency = \"editor\"\n", "echo later ran");
    let group = project.locks.path("project-case/editor");
    fs::create_dir_all(group.join("holders")).unwrap();
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (ready, signal) = (fds[0], fds[1]);
    let gate = std::ffi::CString::new(group.join("gate").to_string_lossy().into_owned()).unwrap();
    let holders = std::ffi::CString::new(group.join("holders").to_string_lossy().into_owned()).unwrap();
    // SAFETY: the child makes only async-signal-safe calls (open, flock, write, pause) before it is killed.
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        unsafe {
            let mut path = [0u8; 1024];
            let directory = holders.as_bytes();
            path[..directory.len()].copy_from_slice(directory);
            let mut at = directory.len();
            path[at] = b'/';
            path[at + 1] = b'.';
            at += 2;
            let mut digits = [0u8; 12];
            let (mut pid, mut count) = (libc::getpid(), 0);
            while pid > 0 {
                digits[count] = b'0' + (pid % 10) as u8;
                pid /= 10;
                count += 1;
            }
            for index in 0..count {
                path[at + index] = digits[count - 1 - index];
            }
            let intent = libc::open(path.as_ptr() as *const libc::c_char, libc::O_CREAT | libc::O_RDWR, 0o644);
            libc::flock(intent, libc::LOCK_EX);
            let gate = libc::open(gate.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644);
            libc::flock(gate, libc::LOCK_EX);
            libc::write(signal, b"x".as_ptr() as *const libc::c_void, 1);
            loop {
                libc::pause();
            }
        }
    }
    unsafe {
        libc::close(signal);
        let mut byte = 0u8;
        assert_eq!(libc::read(ready, &mut byte as *mut u8 as *mut libc::c_void, 1), 1);
        libc::close(ready);
    }
    let mut waiting =
        project.x(&["later"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let line = stderr_line(waiting.stderr.as_mut().unwrap());
    assert!(line.contains("later waits: editor is held by"), "{line}");
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    let (status, output) = run(caim().args(["exit", &waiting.id().to_string(), "60"]));
    if status != 0 {
        waiting.kill().unwrap();
        waiting.wait().unwrap();
        panic!("the waiting run never noticed the group was free: {output}");
    }
    let (status, output) = finish(waiting);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("later ran"), "{output}");
}

#[test]
fn every_checkout_of_a_project_shares_a_group() {
    let locks = Rc::new(TempDir::new());
    let scratch = TempDir::new();
    let checkouts: Vec<Project> = (0..2).map(|_| Project::named("game", locks.clone())).collect();
    let stranger = Project::named("tool", locks.clone());
    for project in checkouts.iter().chain([&stranger]) {
        project.task("check", "first", "description = \"t\"\nconcurrency = \"editor\"\n", HOLDS);
        project.task("check", "second", &format!("description = \"t\"\n{REFUSING}"), "echo second");
        project.task(
            "check",
            "own",
            "description = \"t\"\n[concurrency]\ngroup = \"editor\"\nscope = \"checkout\"\non_busy = \"refuse\"\n",
            "echo own",
        );
    }
    let (main, linked) = (&checkouts[0], &checkouts[1]);
    let holder = Holder::start(main.x(&["first"]), "first", &scratch.0);

    let (status, stdout, stderr) = split(&mut linked.x(&["second"]));
    assert_eq!(status, 75, "another checkout of the project: {stdout}{stderr}");
    assert!(stderr.contains("held by first (pid "), "{stderr}");
    assert!(stderr.contains(&format!(" in {} since ", main.dir.0.display())), "{stderr}");
    let (status, output) = linked.run(&["own"]);
    assert_eq!(status, 0, "a checkout-scoped group is the checkout's own: {output}");
    let (status, output) = stranger.run(&["second"]);
    assert_eq!(status, 0, "a project of another name: {output}");

    let (status, output) = holder.release();
    assert_eq!(status, 0, "{output}");
    assert_eq!(linked.run(&["second"]).0, 0);
}

#[test]
fn a_project_is_found_from_below_and_must_name_itself() {
    let project = Project::new();
    project.task("check", "where", SIMPLE, "echo \"$PWD\"");
    fs::create_dir_all(project.dir.path("deep/er")).unwrap();
    let mut command = isolated(&project.dir.0, &project.locks.0, &["where"]);
    command.env_remove("CAIM_ROOT").current_dir(project.dir.path("deep/er"));
    let (status, stdout, stderr) = split(&mut command);
    assert_eq!(status, 0, "{stdout}{stderr}");
    assert_eq!(stdout.lines().next(), Some(&*project.dir.0.to_string_lossy()), "tasks run at the project's root");

    let outside = TempDir::new();
    let mut command = isolated(&outside.0, &project.locks.0, &["list"]);
    command.env_remove("CAIM_ROOT").current_dir(&outside.0);
    let (status, output) = run(&mut command);
    assert_eq!(status, 78, "{output}");
    assert!(output.contains("no caim.toml in the current directory or above it"), "{output}");
    for help in ["--help", "-h", "help"] {
        let mut command = isolated(&outside.0, &project.locks.0, &[help]);
        command.env_remove("CAIM_ROOT").current_dir(&outside.0);
        let (status, stdout, stderr) = split(&mut command);
        assert_eq!(status, 0, "{help}: {stdout}{stderr}");
        assert!(stdout.starts_with("usage: caim "), "{help}: {stdout}");
        assert!(stderr.contains("no caim.toml in the current directory or above it"), "{help}: {stderr}");
    }

    for (text, expected) in [
        ("", "missing field `name`"),
        ("name = \"Bad Name\"\n", "name \"Bad Name\" is not lowercase letters"),
        ("name = \"x\"\nversion = 1\n", "unknown field `version`"),
    ] {
        fs::write(project.dir.path("caim.toml"), text).unwrap();
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{text:?}: {output}");
        assert!(output.contains(expected), "{text:?}: {output}");
    }
}

#[test]
fn a_machine_scoped_group_spans_projects() {
    let one = Project::new();
    let other = Project::sharing_locks(one.locks.clone());
    let scratch = TempDir::new();
    let machine = "[concurrency]\ngroup = \"gpu\"\nscope = \"machine\"\non_busy = \"refuse\"\n";
    one.task("check", "render", &format!("description = \"t\"\n{machine}"), HOLDS);
    other.task("check", "bake", &format!("description = \"t\"\n{machine}"), "echo bake");
    let holder = Holder::start(one.x(&["render"]), "render", &scratch.0);
    let (status, stdout, stderr) = split(&mut other.x(&["bake"]));
    assert_eq!(status, 75, "{stdout}{stderr}");
    assert!(stderr.contains("gpu is held by render (pid "), "{stderr}");
    assert!(stderr.contains(&format!("in {}", one.dir.0.display())), "{stderr}");
    let (status, output) = holder.release();
    assert_eq!(status, 0, "{output}");
    assert_eq!(other.run(&["bake"]).0, 0);
}

#[test]
fn shared_holders_run_together_and_keep_an_exclusive_one_out() {
    let project = Project::new();
    let scratch = TempDir::new();
    let shared = "[concurrency]\ngroup = \"binaries\"\nmode = \"shared\"\non_busy = \"refuse\"\n";
    let alone = "[concurrency]\ngroup = \"binaries\"\non_busy = \"refuse\"\n";
    project.task(
        "test",
        "test",
        &format!("description = \"t\"\n{shared}"),
        &format!("[[ -n \"${{READY:-}}\" ]] || exit 0\n{HOLDS}"),
    );
    project.task(
        "check",
        "build",
        &format!("description = \"t\"\n{alone}"),
        &format!("[[ -n \"${{READY:-}}\" ]] || exit 0\n{HOLDS}"),
    );
    project.task("check", "lint", SIMPLE, "echo lint");

    let one = Holder::start(project.x(&["test"]), "one", &scratch.0);
    let two = Holder::start(project.x(&["test"]), "two", &scratch.0);
    let (status, stdout, stderr) = split(&mut project.x(&["build"]));
    assert_eq!(status, 75, "a build waits for the tests to leave: {stdout}{stderr}");
    assert_eq!(stderr.matches("test (pid ").count(), 2, "{stderr}");
    assert_eq!(project.run(&["lint"]).0, 0, "an undeclared task runs alongside");
    assert_eq!(one.release().0, 0);
    assert_eq!(two.release().0, 0);

    let build = Holder::start(project.x(&["build"]), "build", &scratch.0);
    let (status, output) = project.run(&["test"]);
    assert_eq!(status, 75, "a test waits for the build to leave: {output}");
    assert_eq!(project.run(&["lint"]).0, 0, "an undeclared task runs alongside");
    assert_eq!(build.release().0, 0);
    assert_eq!(project.run(&["test"]).0, 0);
}

#[test]
fn a_limit_admits_that_many_shared_holders() {
    let project = Project::new();
    let scratch = TempDir::new();
    project.task(
        "check",
        "slot",
        "description = \"t\"\n[concurrency]\ngroup = \"device\"\nmode = \"shared\"\nlimit = 2\non_busy = \"refuse\"\n",
        &format!("[[ -n \"${{READY:-}}\" ]] || {{ echo free; exit 0; }}\n{HOLDS}"),
    );
    let first = Holder::start(project.x(&["slot"]), "one", &scratch.0);
    let second = Holder::start(project.x(&["slot"]), "two", &scratch.0);
    let (status, stdout, stderr) = split(&mut project.x(&["slot"]));
    assert_eq!(status, 75, "{stdout}{stderr}");
    assert_eq!(stderr.matches("slot (pid ").count(), 2, "{stderr}");
    let (status, output) = first.release();
    assert_eq!(status, 0, "{output}");
    let (status, output) = project.run(&["slot"]);
    assert_eq!(status, 0, "a slot is free again: {output}");
    assert_eq!(second.release().0, 0);
}

#[test]
fn a_group_already_held_by_this_run_does_not_block_it() {
    let project = Project::new();
    project.task("check", "base", &format!("description = \"t\"\n{REFUSING}"), "echo base");
    project.task(
        "check",
        "top",
        "description = \"t\"\nconcurrency = \"editor\"\nrequires = [\"base\"]\n",
        "\"$CAIM\" base",
    );
    let (status, output) = project.run(&["top"]);
    assert_eq!(status, 0, "{output}");
    assert_eq!(output.matches("[base] exit=0").count(), 2, "{output}");
}

#[test]
fn a_holder_that_dies_frees_its_group() {
    let project = Project::new();
    let scratch = TempDir::new();
    project.task("check", "first", "description = \"t\"\nconcurrency = \"editor\"\n", HOLDS);
    project.task("check", "second", &format!("description = \"t\"\n{REFUSING}"), "echo second");
    let mut holder = Holder::start(project.x(&["first"]), "first", &scratch.0);
    let command: i32 = fs::read_to_string(&holder.ready).unwrap().trim().parse().unwrap();
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    let (status, output) = project.run(&["second"]);
    unsafe { libc::kill(-command, libc::SIGKILL) };
    assert_eq!(status, 0, "{output}");
}

#[test]
fn a_bad_concurrency_declaration_is_setup() {
    for (toml, expected) in [
        ("description = \"t\"\nconcurrency = \"Bad Name\"\n", "concurrency group \"Bad Name\""),
        ("description = \"t\"\n[concurrency]\ngroup = \"x\"\non_busy = \"cancel\"\n", "unknown variant `cancel`"),
        ("description = \"t\"\n[concurrency]\ngroup = \"x\"\nscope = \"world\"\n", "unknown variant `world`"),
        (
            "description = \"t\"\n[concurrency]\ngroup = \"x\"\nmode = \"shared\"\nlimit = 0\n",
            "limit must be at least 1",
        ),
        ("description = \"t\"\n[concurrency]\ngroup = \"x\"\nlimit = 2\n", "limit applies to shared holders only"),
        ("description = \"t\"\n[concurrency]\ngruop = \"x\"\n", "unknown field `gruop`"),
        ("description = \"t\"\nconcurrency = 3\n", "a group name, a [concurrency] table, or a list of them"),
        ("description = \"t\"\nconcurrency = [\"a\", 3]\n", "a group name or a [concurrency] table"),
    ] {
        let project = Project::new();
        project.task("check", "bad", toml, "exit 0");
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{toml}: {output}");
        assert!(output.contains(expected), "{toml}: {output}");
    }
}

fn read_terminal_until(master: i32, until: &str) -> Result<String, String> {
    let mut text = Vec::new();
    loop {
        let mut poll = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
        let ready = unsafe { libc::poll(&mut poll, 1, 120_000) };
        let mut buffer = [0u8; 4096];
        let count = if ready > 0 {
            unsafe { libc::read(master, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) }
        } else {
            0
        };
        if count <= 0 {
            return Err(String::from_utf8_lossy(&text).into_owned());
        }
        text.extend_from_slice(&buffer[..count as usize]);
        if String::from_utf8_lossy(&text).contains(until) {
            return Ok(String::from_utf8_lossy(&text).into_owned());
        }
    }
}

#[test]
fn an_unguarded_task_reads_from_the_terminal() {
    let project = Project::new();
    project.task("dev", "ask", "description = \"a\"\nguard = false\n", "read answer; echo \"got $answer\"");
    let (mut master, mut slave) = (0, 0);
    let opened = unsafe {
        libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
    };
    assert_eq!(opened, 0, "no pseudo-terminal could be opened here: {}", std::io::Error::last_os_error());
    let mut command = project.x(&["ask"]);
    unsafe {
        use std::os::fd::FromRawFd;
        command
            .stdin(Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(Stdio::from_raw_fd(libc::dup(slave)));
        // Only async-signal-safe calls between fork and exec: a session of its own, with the
        // terminal as its controlling terminal and its group in the foreground.
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    unsafe { libc::close(slave) };
    unsafe { libc::write(master, b"yes\n".as_ptr() as *const libc::c_void, 4) };
    let text = match read_terminal_until(master, "] exit=") {
        Ok(text) => text,
        Err(text) => {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            child.wait().unwrap();
            unsafe { libc::close(master) };
            panic!("the run hung or the terminal closed before its exit line: {text}");
        }
    };
    let status = child.wait().unwrap();
    unsafe { libc::close(master) };
    assert!(text.contains("got yes"), "{text}");
    assert!(text.contains("[ask] exit=0"), "{text}");
    assert_eq!(status.code(), Some(0), "{text}");
}

#[test]
fn a_task_declares_its_idle_time_and_stop_steps() {
    let project = Project::new();
    project.task(
        "check",
        "quiet",
        "description = \"q\"\nidle = 1\nstop = [\"INT:20\", \"KILL:20\"]\n",
        "echo started; exec tail -f /dev/null",
    );
    let (status, stdout, stderr) = split(project.x(&["quiet"]).env("CAIM_STALL_SECONDS", "600"));
    assert_eq!(status, 124, "{stdout}{stderr}");
    assert!(stderr.contains("no progress for 1 s"), "{stderr}");
    assert!(stderr.contains("[stop] INT to 1 process(es)"), "{stderr}");
    assert_eq!(last_line(&stdout), "[quiet] exit=124 (stalled)", "{stdout}");
    for (toml, expected) in [
        ("description = \"t\"\nidle = 0\n", "idle is a positive number of seconds"),
        ("description = \"t\"\nstop = []\n", "stop needs at least one step"),
        ("description = \"t\"\nstop = [\"TERM\"]\n", "stop step \"TERM\" is not SIGNAL:SECONDS"),
        ("description = \"t\"\nstop = [\"USR1:5\"]\n", "stop step \"USR1:5\" is not SIGNAL:SECONDS"),
    ] {
        let project = Project::new();
        project.task("check", "bad", toml, "exit 0");
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{toml}: {output}");
        assert!(output.contains(expected), "{toml}: {output}");
    }
}

#[test]
fn a_tasks_env_and_dir_are_its_own() {
    let project = Project::new();
    fs::create_dir_all(project.dir.path("sub")).unwrap();
    project.task(
        "build",
        "inside",
        "description = \"i\"\ndir = \"sub\"\n[env]\nGREETING = \"hello there\"\n",
        "echo \"inside: ${GREETING:-none} in $(basename \"$PWD\")\"",
    );
    project.task("build", "after", SIMPLE, "echo \"after: ${GREETING:-none} in $(basename \"$PWD\")\"");
    let (status, output) = project.run(&["inside", "after"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("inside: hello there in sub"), "{output}");
    let root = project.dir.0.file_name().unwrap().to_string_lossy().into_owned();
    assert!(output.contains(&format!("after: none in {root}")), "{output}");
    project.untouched(&["sub"]);
    for (toml, expected) in [
        ("description = \"t\"\ndir = \"/tmp\"\n", "dir is a path inside the project"),
        ("description = \"t\"\ndir = \"../elsewhere\"\n", "dir is a path inside the project"),
        ("description = \"t\"\n[env]\nCAIM_ARG_X = \"1\"\n", "env cannot set \"CAIM_ARG_X\""),
        ("description = \"t\"\n[env]\n\"A=B\" = \"1\"\n", "env cannot set \"A=B\""),
    ] {
        let project = Project::new();
        project.task("check", "bad", toml, "exit 0");
        let (status, output) = project.run(&["list"]);
        assert_eq!(status, 78, "{toml}: {output}");
        assert!(output.contains(expected), "{toml}: {output}");
    }
    let project = Project::new();
    project.task("check", "nowhere", "description = \"t\"\ndir = \"missing\"\n", "exit 0");
    let (status, output) = project.run(&["nowhere"]);
    assert_eq!(status, 78, "{output}");
    assert!(output.contains("nowhere did not start; its dir missing is not a directory"), "{output}");
}

#[test]
fn a_task_runs_what_its_requirement_builds() {
    let project = Project::new();
    project.task(
        "build",
        "build",
        SIMPLE,
        "mkdir -p out && printf '#!/bin/sh\\necho built tool ran\\n' > out/tool && chmod +x out/tool",
    );
    project.written("\n[tasks.use]\nrequires = [\"build\"]\nrun = [\"out/tool\"]\n");
    let (status, output) = project.run(&["list"]);
    assert_eq!(status, 0, "{output}");
    let (status, output) = project.run(&["use"]);
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("built tool ran"), "{output}");
    assert_eq!(last_line(&output), "[use] exit=0", "{output}");
}

#[test]
fn tasks_on_one_line_keep_their_environments_apart() {
    let project = Project::new();
    project.hook("gate", "echo \"gate sees platform=${CAIM_ARG_PLATFORM:-none}\"");
    project.task(
        "build",
        "first",
        "description = \"f\"\n[[args]]\nname = \"platform\"\nhelp = \"p\"\n",
        "echo \"first sees platform=${CAIM_ARG_PLATFORM:-none}\"",
    );
    project.task(
        "build",
        "second",
        "description = \"s\"\nlock = \"gate\"\n",
        "echo \"second sees platform=${CAIM_ARG_PLATFORM:-none}\"",
    );
    let temporary = TempDir::new();
    let (status, output) = run(project.x(&["first", "--platform", "Win64", "second"]).env("TMPDIR", &temporary.0));
    assert_eq!(status, 0, "{output}");
    assert!(output.contains("first sees platform=Win64"), "{output}");
    assert!(output.contains("gate sees platform=none"), "{output}");
    assert!(output.contains("second sees platform=none"), "{output}");
    let left: Vec<_> = fs::read_dir(&temporary.0).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert!(left.is_empty(), "the run left {left:?} in TMPDIR");
}

fn completion_project() -> Project {
    let project = Project::new();
    project.task(
        "release",
        "package",
        "description = \"p\"\n[[args]]\nname = \"platform\"\nhelp = \"p\"\nchoices = [\"Mac\", \"Win64\"]\n[[args]]\nname = \"fast\"\nhelp = \"f\"\nflag = true\n",
        "exit 0",
    );
    project.task("build", "build", SIMPLE, "exit 0");
    project.task("build", "internal", "description = \"i\"\nhidden = true\n", "exit 0");
    project
}

#[test]
fn completion_words_follow_the_command_line() {
    let project = completion_project();
    let words = |line: &[&str]| -> Vec<String> {
        let mut args = vec!["completions", "words"];
        args.extend(line);
        let (status, output) = project.run(&args);
        assert_eq!(status, 0, "{output}");
        output.lines().map(String::from).collect()
    };
    assert_eq!(words(&[]), ["list", "--help", "--dry-run", "build", "package"]);
    assert_eq!(words(&["package"]), ["--platform", "--fast", "--help", "build", "package"]);
    assert_eq!(words(&["package", "--platform"]), ["Mac", "Win64"]);
    assert_eq!(words(&["package", "--fast"]), ["--platform", "--fast", "--help", "build", "package"]);
    assert!(words(&["list"]).is_empty());
    assert_eq!(words(&["completions"]), ["bash", "zsh"]);
    let outside = TempDir::new();
    let (status, output) = run(caim().args(["completions", "words"]).current_dir(&outside.0).env_remove("CAIM_ROOT"));
    assert_eq!((status, output.as_str()), (0, ""), "outside a project nothing is offered");
}

#[test]
fn the_bash_and_zsh_scripts_complete_task_names() {
    let project = completion_project();
    let binary = env!("CARGO_BIN_EXE_caim");
    let bash = "eval \"$(\"$CAIM\" completions bash)\"; COMP_WORDS=(\"$CAIM\" pa); COMP_CWORD=1; _caim; printf '%s\\n' \"${COMPREPLY[@]}\"";
    let (status, output) =
        run(Command::new("bash").args(["-c", bash]).env("CAIM", binary).env("CAIM_ROOT", &project.dir.0));
    assert_eq!((status, output.as_str()), (0, "package\n"));
    let zsh = "compdef() { :; }; compadd() { print -l -- \"${(@P)2}\"; }; eval \"$(\"$CAIM\" completions zsh)\"; words=(\"$CAIM\" package --platform ''); CURRENT=4; _caim";
    let (status, output) =
        run(Command::new("zsh").args(["-f", "-c", zsh]).env("CAIM", binary).env("CAIM_ROOT", &project.dir.0));
    assert_eq!((status, output.as_str()), (0, "Mac\nWin64\n"));
}
