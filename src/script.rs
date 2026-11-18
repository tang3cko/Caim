//! `caim script FILE` runs a Lua script. Every wait a script makes is a coroutine yield, and one
//! native queue serves all of them, so a script waits on several processes and log lines at once
//! without threads, sleeps or polling.

use crate::error::{Error, FAILED, Result, USAGE, missing};
use crate::native::{self, Ended, Interest, Queue, Report, RunPlan, StopStep, Watch, WatchPlan};
use crate::waits::{LineWatch, absolute, stop_step};
use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, MultiValue, Table, Thread, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
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

local rank = { PASS = 0, FAIL = 1, VOID = 2 }
local names = { [0] = "PASS", "FAIL", "VOID" }
local Verdict = {}
Verdict.__index = Verdict

function caim.verdict()
  return setmetatable({ cases = {}, order = {}, expected = {} }, Verdict)
end

function Verdict:record(verdict, name)
  if type(name) ~= "string" then error("a case is named by a string", 2) end
  verdict = rank[verdict] and verdict or "VOID"
  local seen = self.cases[name]
  if seen == nil then self.order[#self.order + 1] = name end
  if seen == nil or rank[verdict] > rank[seen] then self.cases[name] = verdict end
  return self
end

function Verdict:pass(name) return self:record("PASS", name) end
function Verdict:fail(name) return self:record("FAIL", name) end
function Verdict:void(name) return self:record("VOID", name) end

function Verdict:expect(list)
  for _, name in ipairs(list) do self.expected[#self.expected + 1] = name end
  return self
end

function Verdict:finish()
  local listed, shown = {}, {}
  for _, name in ipairs(self.expected) do
    if not shown[name] then shown[name] = true; listed[#listed + 1] = name end
    if self.cases[name] == nil then self:record("VOID", name) end
  end
  for _, name in ipairs(self.order) do
    if not shown[name] then shown[name] = true; listed[#listed + 1] = name end
  end
  local worst, count = #listed == 0 and rank.VOID or rank.PASS, { PASS = 0, FAIL = 0, VOID = 0 }
  for _, name in ipairs(listed) do
    local verdict = self.cases[name]
    print(("VERDICT %s case=%s"):format(verdict, name))
    count[verdict] = count[verdict] + 1
    worst = math.max(worst, rank[verdict])
  end
  print(("VERDICT %s cases=%d pass=%d fail=%d void=%d"):format(names[worst], #listed, count.PASS, count.FAIL, count.VOID))
  return worst
end
"#;

const STOP_STEPS: [StopStep; 2] =
    [StopStep { signal: libc::SIGTERM, grace: 20.0 }, StopStep { signal: libc::SIGKILL, grace: 20.0 }];

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
    deferred: Vec<Function>,
    /// A termination signal that reached a supervised run; the script ends with it.
    interrupted: Option<i32>,
}

/// The script takes the termination signals through its queue, which ignores or blocks them;
/// a process it starts gets the default handling back.
fn default_signals(command: &mut Command) {
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
}

fn steps(given: Option<Vec<String>>) -> mlua::Result<Vec<StopStep>> {
    match given {
        None => Ok(STOP_STEPS.to_vec()),
        Some(given) if given.is_empty() => Err(mlua::Error::runtime("stop needs at least one step")),
        Some(given) => given
            .iter()
            .map(|step| stop_step(step))
            .collect::<Result<_>>()
            .map_err(|error| mlua::Error::runtime(error.to_string())),
    }
}

/// A line wait: on the file once it exists, else on the nearest directory above it that does.
enum Target {
    Open { watch: LineWatch, _registered: Watch },
    Absent { path: String, pattern: String, anchor: String, _registered: Watch },
}

enum Wait {
    Exit(i32),
    First(Vec<i32>),
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
            default_signals(&mut command);
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
        lua.create_function(|_, (pid, given): (i32, Option<Vec<String>>)| {
            let left = native::stop(0, &[pid], &steps(given)?, &mut std::io::stderr())
                .map_err(|error| mlua::Error::runtime(error.to_string()))?;
            Ok(left == 0)
        })?,
    )?;

    let running = shared.clone();
    caim.set(
        "run",
        lua.create_function(move |_, spec: Table| {
            let command: Vec<String> = spec.sequence_values::<String>().collect::<mlua::Result<_>>()?;
            if command.is_empty() {
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
                command,
                output: output.clone(),
                idle: Some(idle),
                watch: WatchPlan { logs: vec![output], ..WatchPlan::default() },
                steps: steps(spec.get("steps")?)?,
                environment,
                ..RunPlan::default()
            };
            // The signals stay with the queues: supervise takes them while it runs, and the
            // script's own queue still holds them after, so no TERM falls between the two.
            let ended = native::supervise(
                &plan,
                Report {
                    started: &mut |_| {},
                    stalled: &mut |pid| eprintln!("{}", native::stall_message(pid, idle)),
                    unavailable: &mut |error| eprintln!("caim: the watch failed: {error}"),
                    stops: &mut std::io::stderr(),
                    output: &mut |_| {},
                },
            )
            .map_err(|error| mlua::Error::runtime(format!("run: {error}")))?;
            let reaped = |status: Option<i32>| status.map(i64::from);
            Ok(match ended {
                Ended::Exit(status) => ("exit", Some(i64::from(status))),
                Ended::Stall(status) => ("stall", reaped(status)),
                Ended::Interrupted(signal, _) => {
                    running.borrow_mut().interrupted = Some(signal);
                    ("interrupted", Some(i64::from(signal)))
                }
                Ended::Unavailable(_, status) => ("unavailable", reaped(status)),
            })
        })?,
    )?;

    caim.set(
        "lines",
        lua.create_function(|lua, (path, pattern): (String, String)| {
            let pattern = regex::Regex::new(&pattern)
                .map_err(|error| mlua::Error::runtime(format!("invalid regular expression: {error}")))?;
            let text = match std::fs::read(&path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) if missing(&error) => String::new(),
                Err(error) => return Err(mlua::Error::runtime(format!("{path}: {error}"))),
            };
            let mut rows: VecDeque<Vec<String>> = VecDeque::new();
            for line in text.lines() {
                if let Some(found) = pattern.captures(line) {
                    let mut row = vec![line.to_string()];
                    row.extend(found.iter().skip(1).map(|group| group.map_or_else(String::new, |m| m.as_str().into())));
                    rows.push_back(row);
                }
            }
            lua.create_function_mut(move |_, ()| Ok(mlua::Variadic::from_iter(rows.pop_front().unwrap_or_default())))
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
            let variable = name.to_ascii_uppercase().replace('-', "_");
            let found = [format!("CAIM_ARG_{variable}"), variable]
                .iter()
                .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()));
            Ok(found.or(default))
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
            absolute(&path).map_err(|error| mlua::Error::runtime(format!("abs: {error}")))
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
    let ended = drive(&lua, &shared, main);
    // Last registered first, whatever ended the script.
    loop {
        let Some(function) = shared.borrow_mut().deferred.pop() else { break };
        if let Err(error) = function.call::<()>(()) {
            eprintln!("caim script: a deferred function failed: {error}");
        }
    }
    ended
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
    queue.termination()?;
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
            if let Some(signal) = shared.borrow().interrupted {
                return Ok(128 + signal);
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
                "first" => {
                    let Some(Value::Table(list)) = values.next() else {
                        return Err(Error::Usage("first needs a list of pids".into()));
                    };
                    let pids: Vec<i32> =
                        list.sequence_values::<i32>().collect::<mlua::Result<_>>().map_err(lua_error)?;
                    if pids.is_empty() {
                        return Err(Error::Usage("first needs a list of pids".into()));
                    }
                    for &pid in &pids {
                        if !shared.borrow().processes.exited.contains(&pid) && !queue.process(pid)? {
                            shared.borrow_mut().processes.mark_exited(pid);
                        }
                    }
                    parked.push(Parked { id, thread, wait: Wait::First(pids) });
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
                "usage" => {
                    let message = text(values.next()).ok_or_else(|| Error::Usage("usage needs a message".into()))?;
                    eprintln!("{message}");
                    return Ok(USAGE);
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
                Wait::First(pids) => {
                    let processes = &shared.borrow().processes;
                    pids.iter().find(|pid| processes.exited.contains(pid)).map(|&pid| Value::Integer(pid.into()))
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
                .flat_map(|waiter| match &waiter.wait {
                    Wait::Exit(pid) => vec![*pid],
                    Wait::First(pids) => pids.clone(),
                    Wait::Line { pid, .. } => pid.iter().copied().collect(),
                    Wait::Join(_) => Vec::new(),
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
        let events = match queue.wait(None) {
            Ok(events) => events,
            Err(Error::Interrupted(signal)) => return Ok(128 + signal),
            Err(error) => return Err(error),
        };
        for event in events {
            if let Some(pid) = native::exited_pid(&event) {
                shared.borrow_mut().processes.mark_exited(pid);
            }
        }
    }
}
