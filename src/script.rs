//! `caim script FILE` runs a Lua script. Every wait a script makes is a coroutine yield and one
//! native queue serves all of them, so a script can wait on several processes and log lines at
//! once without threads, sleeps or polling.

use crate::error::{Error, FAILED, Result, USAGE};
use crate::native::{self, Interest, Queue, Report, RunPlan, StopStep, Watch, WatchPlan};
use crate::waits::{LineWatch, absolute, stop_step};
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, MultiValue, Table, Thread, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

pub const MODE: &str = "script";

const PRELUDE: &str = r#"
local yield = coroutine.yield
local function wait(...)
  if not coroutine.isyieldable() then error("a deferred function cannot wait", 3) end
  return yield(...)
end
function caim.exit(pid) return wait("exit", pid) end
function caim.first(pids) return wait("first", pids) end
function caim.line(path, pattern, pid) return wait("line", path, pattern, pid) end
function caim.join(id) return wait("join", id) end
function caim.usage(message) return wait("usage", message) end
"#;

const STOP_STEPS: [StopStep; 2] =
    [StopStep { signal: libc::SIGTERM, grace: 20.0 }, StopStep { signal: libc::SIGKILL, grace: 20.0 }];

enum Wait {
    Exit(i32),
    First(Vec<i32>),
    Line { watch: LineWatch, _watch: Watch, pid: Option<i32> },
    Join(u64),
}

struct Parked {
    id: u64,
    thread: Thread,
    wait: Wait,
}

struct Shared {
    queue: Rc<Queue>,
    started: Vec<(u64, Function)>,
    children: BTreeMap<i32, Child>,
    deferred: Vec<Function>,
    interrupted: Option<i32>,
    next: u64,
}

impl Shared {
    fn new(queue: Rc<Queue>) -> Self {
        Shared {
            queue,
            started: Vec::new(),
            children: BTreeMap::new(),
            deferred: Vec::new(),
            interrupted: None,
            next: 0,
        }
    }
}

fn runtime(error: Error) -> mlua::Error {
    mlua::Error::runtime(error.to_string())
}

fn lua_error(error: mlua::Error) -> Error {
    Error::Unavailable(format!("lua: {error}"))
}

fn arguments(lua: &Lua) -> mlua::Result<Table> {
    let args = lua.create_table()?;
    for (name, value) in std::env::vars() {
        if let Some(rest) = name.strip_prefix("CAIM_ARG_") {
            args.set(rest.to_ascii_lowercase(), value)?;
        }
    }
    Ok(args)
}

fn install(lua: &Lua, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    let caim = lua.create_table()?;
    caim.set("args", arguments(lua)?)?;
    caim.set("task", std::env::var("CAIM_TASK").unwrap_or_default())?;

    let spawned = shared.clone();
    caim.set(
        "spawn",
        lua.create_function(move |_, spec: Table| {
            let argv: Vec<String> = spec.sequence_values::<String>().collect::<mlua::Result<_>>()?;
            let Some((program, rest)) = argv.split_first() else {
                return Err(mlua::Error::runtime("spawn needs a command"));
            };
            let mut command = Command::new(program);
            command.args(rest).stdin(Stdio::null());
            // SAFETY: only async-signal-safe calls, between fork and exec.
            unsafe {
                command.pre_exec(|| {
                    for signal in native::TERMINATION.into_iter().chain([libc::SIGTSTP, libc::SIGCHLD]) {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    let mut none: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut none);
                    libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
                    Ok(())
                });
            }
            if let Some(path) = spec.get::<Option<String>>("output")? {
                let file = File::create(&path).map_err(|error| mlua::Error::runtime(format!("{path}: {error}")))?;
                command.stdout(file.try_clone().map_err(mlua::Error::external)?).stderr(file);
            }
            if let Some(env) = spec.get::<Option<Table>>("env")? {
                for pair in env.pairs::<String, String>() {
                    let (key, value) = pair?;
                    command.env(key, value);
                }
            }
            let child = command.spawn().map_err(|error| mlua::Error::runtime(format!("{program}: {error}")))?;
            let pid = child.id() as i32;
            spawned.borrow_mut().children.insert(pid, child);
            Ok(pid)
        })?,
    )?;

    let started = shared.clone();
    caim.set(
        "go",
        lua.create_function(move |_, function: Function| {
            let mut shared = started.borrow_mut();
            shared.next += 1;
            let id = shared.next;
            shared.started.push((id, function));
            Ok(id)
        })?,
    )?;

    caim.set(
        "declare",
        lua.create_function(|_, (kind, path): (String, String)| {
            if kind != "log" && kind != "dir" {
                return Err(mlua::Error::runtime("declare takes \"log\" or \"dir\""));
            }
            let Ok(declared) = std::env::var("CAIM_DECLARED") else { return Ok(false) };
            let path = crate::waits::absolute(&path).map_err(|error| mlua::Error::runtime(error.to_string()))?;
            let mut file = OpenOptions::new().append(true).open(&declared).map_err(mlua::Error::external)?;
            writeln!(file, "{kind} {path}").map_err(mlua::Error::external)?;
            Ok(true)
        })?,
    )?;

    let deferring = shared.clone();
    caim.set(
        "defer",
        lua.create_function(move |_, function: Function| {
            deferring.borrow_mut().deferred.push(function);
            Ok(())
        })?,
    )?;

    caim.set(
        "stop",
        lua.create_function(|_, (pid, steps): (i32, Option<Vec<String>>)| {
            let steps = parse_steps(steps)?;
            let left = native::stop(0, &[pid], &steps, &mut std::io::stderr()).map_err(runtime)?;
            Ok(left == 0)
        })?,
    )?;

    let running = shared.clone();
    caim.set(
        "run",
        lua.create_function(move |_, spec: Table| {
            let argv: Vec<String> = spec.sequence_values::<String>().collect::<mlua::Result<_>>()?;
            if argv.is_empty() {
                return Err(mlua::Error::runtime("run needs a command"));
            }
            let output = spec
                .get::<Option<String>>("output")?
                .ok_or_else(|| mlua::Error::runtime("run needs output, the file for the command's output"))?;
            let idle = spec
                .get::<Option<f64>>("idle")?
                .filter(|idle| idle.is_finite() && *idle > 0.0)
                .ok_or_else(|| mlua::Error::runtime("run needs idle, a positive number of seconds"))?;
            let output = absolute(&output).map_err(|error| mlua::Error::runtime(format!("run: {error}")))?;
            let mut environment = Vec::new();
            if let Some(env) = spec.get::<Option<Table>>("env")? {
                for pair in env.pairs::<String, String>() {
                    environment.push(pair?);
                }
            }
            let plan = RunPlan {
                command: argv,
                output: output.clone(),
                idle: Some(idle),
                watch: WatchPlan { logs: vec![output], ..WatchPlan::default() },
                steps: parse_steps(spec.get("steps")?)?,
                environment,
                ..RunPlan::default()
            };
            let ended = native::supervise(
                &plan,
                Report {
                    started: &mut |_| {},
                    stalled: &mut |pid| eprintln!("{}", native::stall_message(pid, idle)),
                    unavailable: &mut |error| eprintln!("caim: the watch failed: {error}"),
                    stops: &mut std::io::stderr(),
                    output: &mut |_| {},
                },
            );
            // The run's own queue handed the termination signals back; this script's takes them again.
            native::restore_signals();
            running.borrow().queue.termination().map_err(runtime)?;
            let ended = ended.map_err(|error| mlua::Error::runtime(format!("run: {error}")))?;
            let reaped = |status: Option<i32>| status.map(i64::from);
            Ok(match ended {
                native::Ended::Exit(status) => ("exit", Some(i64::from(status))),
                native::Ended::Stall(status) => ("stall", reaped(status)),
                native::Ended::Interrupted(signal, _) => {
                    running.borrow_mut().interrupted = Some(signal);
                    ("interrupted", Some(i64::from(signal)))
                }
                native::Ended::Unavailable(_, status) => ("unavailable", reaped(status)),
            })
        })?,
    )?;

    let root = match crate::tasks::root() {
        Ok(root) => root,
        Err(_) => std::env::current_dir().map_err(mlua::Error::external)?,
    };
    caim.set("root", root.to_string_lossy().into_owned())?;

    caim.set(
        "abs",
        lua.create_function(|_, path: String| {
            crate::waits::absolute(&path).map_err(|error| mlua::Error::runtime(format!("abs: {error}")))
        })?,
    )?;

    caim.set(
        "flags",
        lua.create_function(|lua, (args, spec): (Table, Table)| {
            let mut options: Vec<(String, String)> = spec.pairs::<String, String>().collect::<mlua::Result<_>>()?;
            options.sort();
            let words = lua.create_table()?;
            for (name, option) in options {
                if let Some(value) = args.get::<Option<String>>(name)?.filter(|value| !value.is_empty()) {
                    words.push(option)?;
                    words.push(value)?;
                }
            }
            Ok(words)
        })?,
    )?;

    caim.set(
        "env",
        lua.create_function(|_, (name, default): (String, Option<String>)| {
            let spelled = name.to_ascii_uppercase().replace('-', "_");
            let found = [format!("CAIM_ARG_{spelled}"), spelled]
                .iter()
                .find_map(|variable| std::env::var(variable).ok().filter(|value| !value.is_empty()));
            Ok(found.or(default))
        })?,
    )?;

    lua.globals().set("caim", caim)?;
    lua.load(PRELUDE).set_name("prelude").exec()
}

fn parse_steps(steps: Option<Vec<String>>) -> mlua::Result<Vec<StopStep>> {
    match steps {
        None => Ok(STOP_STEPS.to_vec()),
        Some(steps) if steps.is_empty() => Err(mlua::Error::runtime("stop needs at least one step")),
        Some(steps) => steps.iter().map(|step| stop_step(step)).collect::<Result<_>>().map_err(runtime),
    }
}

/// The exit status of a child this script started, reaping it.
fn reap(shared: &Rc<RefCell<Shared>>, pid: i32) -> Result<Value> {
    let Some(mut child) = shared.borrow_mut().children.remove(&pid) else { return Ok(Value::Nil) };
    let status = child.wait().map_err(|error| Error::io("wait", error))?;
    Ok(Value::Integer(status.code().map_or(128 + status_signal(&status), i64::from)))
}

fn status_signal(status: &std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map_or(0, i64::from)
}

pub fn dispatch(args: &[String]) -> Result<i32> {
    let [_, path] = args else { return Err(Error::Usage("expected FILE".into())) };
    let source = std::fs::read_to_string(path).map_err(|error| Error::io(format!("read {path}"), error))?;
    let lua = Lua::new();
    let queue = Rc::new(Queue::new()?);
    queue.termination()?;
    let shared = Rc::new(RefCell::new(Shared::new(queue)));
    install(&lua, &shared).map_err(lua_error)?;
    let main = lua.load(source).set_name(path.as_str()).into_function().map_err(lua_error)?;
    let ended = drive(&lua, &shared, main);
    run_deferred(&shared);
    ended
}

fn run_deferred(shared: &Rc<RefCell<Shared>>) {
    loop {
        let Some(function) = shared.borrow_mut().deferred.pop() else { return };
        if let Err(error) = function.call::<()>(()) {
            eprintln!("caim script: a deferred function failed: {error}");
        }
    }
}

fn drive(lua: &Lua, shared: &Rc<RefCell<Shared>>, main: Function) -> Result<i32> {
    let queue = shared.borrow().queue.clone();
    let mut ready: VecDeque<(u64, Thread, MultiValue)> =
        VecDeque::from([(0, lua.create_thread(main).map_err(lua_error)?, MultiValue::new())]);
    let mut parked: Vec<Parked> = Vec::new();
    let mut finished: BTreeMap<u64, MultiValue> = BTreeMap::new();
    let mut exited: Vec<i32> = Vec::new();
    let mut status = 0;

    loop {
        while let Some((id, thread, input)) = ready.pop_front() {
            let output = match thread.resume::<MultiValue>(input) {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("caim script: {error}");
                    return Ok(FAILED);
                }
            };
            let started = std::mem::take(&mut shared.borrow_mut().started);
            for (id, function) in started {
                ready.push_back((id, lua.create_thread(function).map_err(lua_error)?, MultiValue::new()));
            }
            if let Some(signal) = shared.borrow().interrupted {
                return Ok(128 + signal);
            }
            if thread.status() != ThreadStatus::Resumable {
                if id == 0 {
                    if let Some(Value::Integer(code)) = output.front() {
                        status = *code as i32;
                    }
                    // The main chunk ending ends the script, as a shell script's last line does.
                    return Ok(status);
                }
                let waiting: Vec<usize> = (0..parked.len())
                    .filter(|&index| matches!(parked[index].wait, Wait::Join(target) if target == id))
                    .collect();
                for index in waiting.into_iter().rev() {
                    let waiter = parked.remove(index);
                    ready.push_back((waiter.id, waiter.thread, output.clone()));
                }
                finished.insert(id, output);
                continue;
            }
            let mut values = output.into_iter();
            let kind = match values.next() {
                Some(Value::String(kind)) => kind.to_str().map_err(lua_error)?.to_string(),
                _ => {
                    eprintln!("caim script: a coroutine yielded something other than a caim wait");
                    return Ok(FAILED);
                }
            };
            let integer = |value: Option<Value>| match value {
                Some(Value::Integer(number)) => Some(number as i32),
                _ => None,
            };
            let text = |value: Option<Value>| match value {
                Some(Value::String(text)) => text.to_str().ok().map(|text| text.to_string()),
                _ => None,
            };
            match kind.as_str() {
                "exit" => {
                    let pid = integer(values.next()).ok_or_else(|| Error::Usage("exit needs a pid".into()))?;
                    if exited.contains(&pid) || !queue.process(pid)? {
                        let code = reap(shared, pid)?;
                        ready.push_back((id, thread, MultiValue::from_vec(vec![code])));
                    } else {
                        parked.push(Parked { id, thread, wait: Wait::Exit(pid) });
                    }
                }
                "first" => {
                    let Some(Value::Table(table)) = values.next() else {
                        return Err(Error::Usage("first needs a list of pids".into()));
                    };
                    let pids: Vec<i32> =
                        table.sequence_values::<i32>().collect::<mlua::Result<_>>().map_err(lua_error)?;
                    if pids.is_empty() {
                        return Err(Error::Usage("first needs a list of pids".into()));
                    }
                    for &pid in &pids {
                        if !exited.contains(&pid) && !queue.process(pid)? {
                            exited.push(pid);
                        }
                    }
                    parked.push(Parked { id, thread, wait: Wait::First(pids) });
                }
                "line" => {
                    let path = text(values.next()).ok_or_else(|| Error::Usage("line needs a path".into()))?;
                    let pattern = text(values.next()).ok_or_else(|| Error::Usage("line needs a pattern".into()))?;
                    let pid = integer(values.next());
                    if let Some(pid) = pid
                        && !exited.contains(&pid)
                        && !queue.process(pid)?
                    {
                        exited.push(pid);
                    }
                    let watch = LineWatch::new(&path, &pattern, None)?;
                    let registered = queue
                        .watch_file(&watch.file, Interest::Written)
                        .map_err(|error| Error::io(format!("watch {path}"), error))?;
                    parked.push(Parked { id, thread, wait: Wait::Line { watch, _watch: registered, pid } });
                }
                "join" => {
                    let target = integer(values.next()).ok_or_else(|| Error::Usage("join needs an id".into()))? as u64;
                    match finished.get(&target) {
                        Some(output) => ready.push_back((id, thread, output.clone())),
                        None => parked.push(Parked { id, thread, wait: Wait::Join(target) }),
                    }
                }
                "usage" => {
                    let message = text(values.next()).ok_or_else(|| Error::Usage("usage needs a message".into()))?;
                    eprintln!("{message}");
                    return Ok(USAGE);
                }
                other => return Err(Error::Usage(format!("unknown wait {other}"))),
            }
        }

        // Settle what can be settled without waiting: a line already written, a process already gone.
        let mut index = 0;
        while index < parked.len() {
            let settled = match &mut parked[index].wait {
                Wait::Exit(pid) if exited.contains(pid) => Some(reap(shared, *pid)?),
                Wait::First(pids) => {
                    pids.iter().find(|pid| exited.contains(pid)).map(|&pid| Value::Integer(pid.into()))
                }
                Wait::Line { watch, pid, .. } => {
                    if watch.matches()? {
                        Some(Value::Boolean(true))
                    } else if pid.is_some_and(|pid| exited.contains(&pid)) {
                        Some(Value::Boolean(false))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            match settled {
                Some(value) => {
                    let waiter = parked.remove(index);
                    ready.push_back((waiter.id, waiter.thread, MultiValue::from_vec(vec![value])));
                }
                None => index += 1,
            }
        }
        if !ready.is_empty() {
            continue;
        }
        if parked.iter().all(|waiter| matches!(waiter.wait, Wait::Join(_))) {
            eprintln!("caim script: every coroutine is waiting on another; nothing can wake them");
            return Ok(FAILED);
        }
        let events = match queue.wait(None) {
            Ok(events) => events,
            Err(Error::Interrupted(signal)) => return Ok(128 + signal),
            Err(error) => return Err(error),
        };
        for event in events {
            if let Some(pid) = native::exited_pid(&event) {
                exited.push(pid);
            }
        }
    }
}
