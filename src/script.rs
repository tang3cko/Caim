//! `caim script FILE` runs a Lua script. Every wait a script makes is a coroutine yield, and one
//! native queue serves all of them, so a script waits on several processes and log lines at once
//! without threads, sleeps or polling.

use crate::error::{Error, FAILED, Result, missing};
use crate::native::{self, Interest, Queue, Watch};
use crate::waits::{LineWatch, absolute};
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, MultiValue, Table, Thread, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

pub const MODE: &str = "script";

const PRELUDE: &str = r#"
local yield = coroutine.yield
function caim.exit(pid) return yield("exit", pid) end
function caim.line(path, pattern, pid) return yield("line", path, pattern, pid) end
function caim.join(id) return yield("join", id) end
"#;

/// The processes this script started and what it knows of their ends.
#[derive(Default)]
struct Processes {
    children: BTreeMap<i32, Child>,
    /// Exited and not yet forgotten; a PID leaves once it is reaped and nothing waits on it, so a
    /// later process given the same PID is not taken for the one that ended.
    exited: Vec<i32>,
    reaped: Vec<i32>,
}

impl Processes {
    fn adopt(&mut self, child: Child) -> i32 {
        let pid = child.id() as i32;
        self.exited.retain(|&gone| gone != pid);
        self.reaped.retain(|&gone| gone != pid);
        self.children.insert(pid, child);
        pid
    }

    fn mark_exited(&mut self, pid: i32) {
        if !self.exited.contains(&pid) {
            self.exited.push(pid);
        }
    }

    /// The status of a process that has exited, reaping it; nil for one this script did not start
    /// or has already reaped.
    fn reap(&mut self, pid: i32) -> Result<Value> {
        let Some(mut child) = self.children.remove(&pid) else { return Ok(Value::Nil) };
        let status = child.wait().map_err(|error| Error::io("wait", error))?;
        self.reaped.push(pid);
        use std::os::unix::process::ExitStatusExt;
        let code = status.code().map_or_else(|| 128 + i64::from(status.signal().unwrap_or(0)), i64::from);
        Ok(Value::Integer(code))
    }
}

#[derive(Default)]
struct Shared {
    processes: Processes,
    started: Vec<(u64, Function)>,
    next: u64,
}

/// A line wait: on the file once it exists, else on the nearest directory above it that does.
enum Target {
    Open { watch: LineWatch, _registered: Watch },
    Absent { path: String, pattern: String, anchor: String, _registered: Watch },
}

enum Wait {
    Exit(i32),
    Line { target: Target, pid: Option<i32> },
    Join(u64),
}

struct Parked {
    id: u64,
    thread: Thread,
    wait: Wait,
}

fn lua_error(error: mlua::Error) -> Error {
    Error::Unavailable(format!("lua: {error}"))
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        None | Some(0) => "/".to_string(),
        Some(slash) => path[..slash].to_string(),
    }
}

fn nearest_directory(path: &str) -> Result<String> {
    let mut ancestor = parent_of(path);
    loop {
        match std::fs::metadata(&ancestor) {
            Ok(info) if info.is_dir() => return Ok(ancestor),
            Ok(_) => {}
            Err(error) if missing(&error) => {}
            Err(error) => return Err(Error::io(format!("stat {ancestor}"), error)),
        }
        ancestor = parent_of(&ancestor);
    }
}

impl Target {
    fn new(queue: &Queue, path: &str, pattern: &str) -> Result<Target> {
        let path = absolute(path)?;
        match File::open(&path) {
            Ok(_) => {
                let watch = LineWatch::new(&path, pattern, None)?;
                let registered = queue
                    .watch_file(&watch.file, Interest::Written)
                    .map_err(|error| Error::io(format!("watch {path}"), error))?;
                Ok(Target::Open { watch, _registered: registered })
            }
            Err(error) if missing(&error) => {
                regex::bytes::Regex::new(pattern)
                    .map_err(|error| Error::Usage(format!("invalid regular expression: {error}")))?;
                let anchor = nearest_directory(&path)?;
                let registered = queue
                    .watch(Path::new(&anchor), true, Interest::Ancestor)
                    .map_err(|error| Error::io(format!("watch {anchor}"), error))?;
                Ok(Target::Absent { path, pattern: pattern.to_string(), anchor, _registered: registered })
            }
            Err(error) => Err(Error::io(format!("open {path}"), error)),
        }
    }

    /// Whether a line has matched, opening the file or moving the watch deeper when it has come
    /// closer to existing.
    fn matches(&mut self, queue: &Queue) -> Result<bool> {
        if let Target::Absent { path, pattern, anchor, .. } = self {
            let (path, pattern, anchor) = (path.clone(), pattern.clone(), anchor.clone());
            match Target::new(queue, &path, &pattern)? {
                opened @ Target::Open { .. } => *self = opened,
                deeper @ Target::Absent { .. } => {
                    if let Target::Absent { anchor: now, .. } = &deeper
                        && *now != anchor
                    {
                        *self = deeper;
                    }
                    return Ok(false);
                }
            }
        }
        match self {
            Target::Open { watch, .. } => watch.matches(),
            Target::Absent { .. } => Ok(false),
        }
    }
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

    let spawning = shared.clone();
    caim.set(
        "spawn",
        lua.create_function(move |_, spec: Table| {
            let argv: Vec<String> = spec.sequence_values::<String>().collect::<mlua::Result<_>>()?;
            let Some((program, rest)) = argv.split_first() else {
                return Err(mlua::Error::runtime("spawn needs a command"));
            };
            let mut command = Command::new(program);
            command.args(rest).stdin(Stdio::null());
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
            Ok(spawning.borrow_mut().processes.adopt(child))
        })?,
    )?;

    let starting = shared.clone();
    caim.set(
        "go",
        lua.create_function(move |_, function: Function| {
            let mut shared = starting.borrow_mut();
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
            let path = absolute(&path).map_err(|error| mlua::Error::runtime(error.to_string()))?;
            let mut file = OpenOptions::new().append(true).open(&declared).map_err(mlua::Error::external)?;
            writeln!(file, "{kind} {path}").map_err(mlua::Error::external)?;
            Ok(true)
        })?,
    )?;

    lua.globals().set("caim", caim)?;
    lua.load(PRELUDE).set_name("prelude").exec()
}

pub fn dispatch(args: &[String]) -> Result<i32> {
    let [_, path] = args else { return Err(Error::Usage("expected FILE".into())) };
    let source = std::fs::read_to_string(path).map_err(|error| Error::io(format!("read {path}"), error))?;
    let lua = Lua::new();
    let shared = Rc::new(RefCell::new(Shared::default()));
    install(&lua, &shared).map_err(lua_error)?;
    let main = lua.load(source).set_name(path.as_str()).into_function().map_err(lua_error)?;
    drive(&lua, &shared, main)
}

fn integer(value: Option<Value>) -> Option<i32> {
    match value {
        Some(Value::Integer(number)) => i32::try_from(number).ok(),
        _ => None,
    }
}

fn text(value: Option<Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => text.to_str().ok().map(|text| text.to_string()),
        _ => None,
    }
}

fn drive(lua: &Lua, shared: &Rc<RefCell<Shared>>, main: Function) -> Result<i32> {
    let queue = Queue::new()?;
    let mut ready: VecDeque<(u64, Thread, MultiValue)> =
        VecDeque::from([(0, lua.create_thread(main).map_err(lua_error)?, MultiValue::new())]);
    let mut parked: Vec<Parked> = Vec::new();
    let mut finished: BTreeMap<u64, MultiValue> = BTreeMap::new();

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
            if thread.status() != ThreadStatus::Resumable {
                if id == 0 {
                    // The main chunk ending ends the script, as a shell script's last line does.
                    return Ok(match output.front() {
                        Some(Value::Integer(code)) => *code as i32,
                        _ => 0,
                    });
                }
                let mut index = 0;
                while index < parked.len() {
                    if matches!(parked[index].wait, Wait::Join(target) if target == id) {
                        let waiter = parked.remove(index);
                        ready.push_back((waiter.id, waiter.thread, output.clone()));
                    } else {
                        index += 1;
                    }
                }
                finished.insert(id, output);
                continue;
            }
            let mut values = output.into_iter();
            let Some(kind) = text(values.next()) else {
                eprintln!("caim script: a coroutine yielded something other than a caim wait");
                return Ok(FAILED);
            };
            match kind.as_str() {
                "exit" => {
                    let pid = integer(values.next()).ok_or_else(|| Error::Usage("exit needs a pid".into()))?;
                    let gone = shared.borrow().processes.exited.contains(&pid) || !queue.process(pid)?;
                    if gone {
                        let mut shared = shared.borrow_mut();
                        shared.processes.mark_exited(pid);
                        let code = shared.processes.reap(pid)?;
                        ready.push_back((id, thread, MultiValue::from_vec(vec![code])));
                    } else {
                        parked.push(Parked { id, thread, wait: Wait::Exit(pid) });
                    }
                }
                "line" => {
                    let path = text(values.next()).ok_or_else(|| Error::Usage("line needs a path".into()))?;
                    let pattern = text(values.next()).ok_or_else(|| Error::Usage("line needs a pattern".into()))?;
                    let pid = integer(values.next());
                    if let Some(pid) = pid
                        && !shared.borrow().processes.exited.contains(&pid)
                        && !queue.process(pid)?
                    {
                        shared.borrow_mut().processes.mark_exited(pid);
                    }
                    let target = Target::new(&queue, &path, &pattern)?;
                    parked.push(Parked { id, thread, wait: Wait::Line { target, pid } });
                }
                "join" => {
                    let target = integer(values.next()).ok_or_else(|| Error::Usage("join needs an id".into()))? as u64;
                    match finished.get(&target) {
                        Some(output) => ready.push_back((id, thread, output.clone())),
                        None => parked.push(Parked { id, thread, wait: Wait::Join(target) }),
                    }
                }
                other => return Err(Error::Usage(format!("unknown wait {other}"))),
            }
        }

        // Settle what can be settled without waiting: a line already written, a process gone.
        let mut index = 0;
        while index < parked.len() {
            let settled = match &mut parked[index].wait {
                Wait::Exit(pid) if shared.borrow().processes.exited.contains(pid) => {
                    Some(shared.borrow_mut().processes.reap(*pid)?)
                }
                Wait::Line { target, pid } => {
                    if target.matches(&queue)? {
                        Some(Value::Boolean(true))
                    } else if pid.is_some_and(|pid| shared.borrow().processes.exited.contains(&pid)) {
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
        {
            let mut shared = shared.borrow_mut();
            let processes = &mut shared.processes;
            let waited: Vec<i32> = parked
                .iter()
                .filter_map(|waiter| match &waiter.wait {
                    Wait::Exit(pid) => Some(*pid),
                    Wait::Line { pid, .. } => *pid,
                    Wait::Join(_) => None,
                })
                .collect();
            let forgotten: Vec<i32> = processes.reaped.iter().copied().filter(|pid| !waited.contains(pid)).collect();
            processes.reaped.retain(|pid| waited.contains(pid));
            processes.exited.retain(|pid| !forgotten.contains(pid));
        }
        if !ready.is_empty() {
            continue;
        }
        if parked.iter().all(|waiter| matches!(waiter.wait, Wait::Join(_))) {
            eprintln!("caim script: every coroutine is waiting on another; nothing can wake them");
            return Ok(FAILED);
        }
        for event in queue.wait(None)? {
            if let Some(pid) = native::exited_pid(&event) {
                shared.borrow_mut().processes.mark_exited(pid);
            }
        }
    }
}
