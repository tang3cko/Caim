use crate::concurrency::{self, Concurrency, Holder, Outcome};
use crate::error::{BUSY, CANNOT_EXECUTE, CONFIG, Error, FAILED, NOT_FOUND, OK, Result, STALLED, USAGE, not_started};
use crate::native::{self, Ended, Report, RunPlan, StopStep, WatchPlan};
use crate::{digests, waits};
use regex::Regex;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const STOP_STEPS: [StopStep; 2] =
    [StopStep { signal: libc::SIGTERM, grace: 20.0 }, StopStep { signal: libc::SIGKILL, grace: 20.0 }];

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    #[serde(default)]
    pub logs: Vec<String>,
    #[serde(default)]
    pub dirs: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Arg {
    pub name: String,
    pub help: String,
    #[serde(default)]
    pub choices: Vec<String>,
    pub default: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub flag: bool,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub guard: bool,
    pub lock: Option<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub human: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub watch: Watch,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[serde(default, deserialize_with = "concurrency::declared")]
    pub concurrency: Vec<Concurrency>,
    pub run: Run,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub idle: Option<f64>,
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum Run {
    Shell(String),
    Command(Vec<String>),
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub run: Run,
}

pub type Commands = BTreeMap<String, Vec<String>>;

#[derive(Clone, Debug)]
pub struct Task {
    pub name: String,
    pub declaration: Declaration,
    pub command: Vec<String>,
    pub source: String,
}

fn setup(message: impl Into<String>) -> Error {
    Error::Unavailable(message.into())
}

pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn reserved(name: &str) -> bool {
    waits::MODES.contains(&name)
        || digests::MODES.contains(&name)
        || ["list", "help", crate::completions::MODE].contains(&name)
}

fn executable(path: &Path) -> bool {
    let Ok(name) = CString::new(path.as_os_str().as_encoded_bytes()) else { return false };
    unsafe { libc::access(name.as_ptr(), libc::X_OK) == 0 }
}

fn check(name: &str, declaration: &Declaration, at: &str, hooks: &Commands) -> Result<()> {
    if !valid_name(name) || reserved(name) {
        return Err(setup(format!(
            "{at}: a task name is lowercase letters, digits and dashes, and not a runner command"
        )));
    }
    if let Some(group) = declaration.group.as_deref().filter(|group| !valid_name(group)) {
        return Err(setup(format!("{at}: group {group:?} is not lowercase letters, digits and dashes")));
    }
    for group in &declaration.concurrency {
        group.check().map_err(|problem| setup(format!("{at}: {problem}")))?;
    }
    for hook in declaration.lock.iter().chain(&declaration.after) {
        if !hooks.contains_key(hook) {
            return Err(setup(format!("{at}: no hook {hook} in [hooks]")));
        }
    }
    if declaration.idle.is_some_and(|idle| !idle.is_finite() || idle <= 0.0) {
        return Err(setup(format!("{at}: idle is a positive number of seconds")));
    }
    if let Some(stop) = &declaration.stop {
        if stop.is_empty() {
            return Err(setup(format!("{at}: stop needs at least one step")));
        }
        for step in stop {
            if stop_step(step).is_none() {
                return Err(setup(format!(
                    "{at}: stop step {step:?} is not SIGNAL:SECONDS (INT, TERM, HUP, QUIT or KILL)"
                )));
            }
        }
    }
    if let Some(dir) = &declaration.dir {
        let inside = Path::new(dir)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_) | std::path::Component::CurDir));
        if dir.is_empty() || !inside {
            return Err(setup(format!("{at}: dir is a path inside the project, not {dir:?}")));
        }
    }
    for key in declaration.env.keys() {
        if key.is_empty() || key.contains('=') || key.contains('\0') || key.starts_with("CAIM_") {
            return Err(setup(format!("{at}: env cannot set {key:?}; CAIM_ names belong to caim")));
        }
    }
    let mut seen = BTreeSet::new();
    for arg in &declaration.args {
        let problem = if !valid_name(&arg.name) || arg.name == "help" {
            Some("is not a valid argument name")
        } else if !seen.insert(arg.name.clone()) {
            Some("is declared twice")
        } else if arg.flag && (arg.required || arg.default.is_some() || !arg.choices.is_empty()) {
            Some("is a flag, which takes no default, choices or required")
        } else if arg.default.as_ref().is_some_and(|value| !arg.choices.is_empty() && !arg.choices.contains(value)) {
            Some("has a default that is not one of its choices")
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(setup(format!("{at}: argument {:?} {problem}", arg.name)));
        }
    }
    Ok(())
}

fn command(root: &Path, run: &Run, at: &str, check: bool) -> Result<Vec<String>> {
    let mut command = match run {
        Run::Shell(line) => return Ok(vec!["/bin/sh".into(), "-c".into(), line.clone()]),
        Run::Command(command) if !command.is_empty() => command.clone(),
        Run::Command(_) => return Err(setup(format!("{at}: run is an empty list"))),
    };
    if command[0].contains('/') {
        let path = root.join(&command[0]);
        if check && !executable(&path) {
            return Err(setup(format!("{at}: {} is missing or not executable", command[0])));
        }
        command[0] = path.to_string_lossy().into_owned();
    }
    Ok(command)
}

fn stop_step(text: &str) -> Option<StopStep> {
    let (name, grace) = text.split_once(':')?;
    let grace: f64 = grace.parse().ok()?;
    (grace.is_finite() && grace >= 0.0).then_some(StopStep { signal: native::signal_number(name)?, grace })
}

fn stop_steps(task: &Task) -> Vec<StopStep> {
    match &task.declaration.stop {
        Some(steps) => steps.iter().filter_map(|step| stop_step(step)).collect(),
        None => STOP_STEPS.to_vec(),
    }
}

fn missing_program(task: &Task) -> Option<&str> {
    let Run::Command(declared) = &task.declaration.run else { return None };
    let first = declared.first()?;
    (first.contains('/') && !executable(Path::new(&task.command[0]))).then_some(first.as_str())
}

pub fn discover(
    root: &Path,
    hooks: BTreeMap<String, Hook>,
    declared: BTreeMap<String, Declaration>,
) -> Result<(Commands, BTreeMap<String, Task>)> {
    let mut commands = BTreeMap::new();
    for (name, hook) in hooks {
        let at = format!("{PROJECT_FILE} [hooks.{name}]");
        if !valid_name(&name) {
            return Err(setup(format!("{at}: a hook name is lowercase letters, digits and dashes")));
        }
        commands.insert(name, command(root, &hook.run, &at, true)?);
    }
    let mut tasks = BTreeMap::new();
    for (name, declaration) in declared {
        let source = format!("{PROJECT_FILE} [tasks.{name}]");
        check(&name, &declaration, &source, &commands)?;
        let command = command(root, &declaration.run, &source, false)?;
        tasks.insert(name.clone(), Task { name, declaration, command, source });
    }
    for task in tasks.values() {
        for required in &task.declaration.requires {
            let Some(other) = tasks.get(required) else {
                return Err(setup(format!("{}: requires unknown task {required}", task.name)));
            };
            if other.declaration.args.iter().any(|arg| arg.required) {
                return Err(setup(format!("{}: requires {required}, which has a required argument", task.name)));
            }
        }
    }
    fn visit(name: &str, tasks: &BTreeMap<String, Task>, path: &mut Vec<String>) -> Result<()> {
        if path.iter().any(|seen| seen == name) {
            path.push(name.to_string());
            return Err(setup(format!("requirement cycle: {}", path.join(" -> "))));
        }
        path.push(name.to_string());
        for required in &tasks[name].declaration.requires {
            visit(required, tasks, path)?;
        }
        path.pop();
        Ok(())
    }
    for name in tasks.keys() {
        visit(name, &tasks, &mut Vec::new())?;
    }
    Ok((commands, tasks))
}

pub fn parse_args(task: &Task, args: &[String]) -> std::result::Result<BTreeMap<String, String>, String> {
    let declared: BTreeMap<&str, &Arg> = task.declaration.args.iter().map(|arg| (arg.name.as_str(), arg)).collect();
    let mut values = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let text = &args[i];
        let Some(option) = text.strip_prefix("--") else {
            return Err(format!("unexpected argument {text:?}; arguments are --name value"));
        };
        let (name, inline) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (option, None),
        };
        let Some(arg) = declared.get(name) else { return Err(format!("unknown option --{name}")) };
        let value = if arg.flag {
            if inline.is_some() {
                return Err(format!("--{name} is a flag and takes no value"));
            }
            "1".to_string()
        } else if let Some(value) = inline {
            value
        } else {
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("--{name} needs a value"))?
        };
        if !arg.choices.is_empty() && !arg.choices.contains(&value) {
            return Err(format!("--{name} must be one of {}, not {value:?}", arg.choices.join(", ")));
        }
        if values.insert(name.to_string(), value).is_some() {
            return Err(format!("--{name} given twice"));
        }
        i += 1;
    }
    for arg in &task.declaration.args {
        if values.contains_key(&arg.name) {
            continue;
        }
        if arg.required {
            return Err(format!("--{} is required", arg.name));
        }
        values.insert(arg.name.clone(), arg.default.clone().unwrap_or_default());
    }
    Ok(values)
}

struct Named<'t> {
    tasks: Vec<(&'t Task, Vec<String>)>,
    help: bool,
}

fn name_tasks<'t>(tasks: &'t BTreeMap<String, Task>, args: &[String]) -> std::result::Result<Named<'t>, String> {
    let mut named = Named { tasks: Vec::new(), help: false };
    let mut i = 0;
    while i < args.len() {
        let text = &args[i];
        i += 1;
        match text.as_str() {
            "--help" | "-h" => named.help = true,
            _ if !text.starts_with('-') => match tasks.get(text) {
                Some(task) => named.tasks.push((task, Vec::new())),
                None => return Err(format!("unknown task {text:?}; caim list shows them")),
            },
            _ => {
                let Some((task, options)) = named.tasks.last_mut() else {
                    return Err(format!("{text} comes before any task; options follow the task they are for"));
                };
                options.push(text.clone());
                let option =
                    text.strip_prefix("--").map(|option| option.split_once('=').map_or(option, |(name, _)| name));
                let declared = option.and_then(|name| task.declaration.args.iter().find(|arg| arg.name == name));
                let Some(declared) = declared else {
                    return Err(format!("{}: unknown option {text}\n\n{}", task.name, usage(task)));
                };
                if !declared.flag && !text.contains('=') && i < args.len() {
                    options.push(args[i].clone());
                    i += 1;
                }
            }
        }
    }
    Ok(named)
}

fn env_name(arg: &str) -> String {
    format!("CAIM_ARG_{}", arg.to_ascii_uppercase().replace('-', "_"))
}

fn arguments(values: &BTreeMap<String, String>) -> Vec<(String, String)> {
    values.iter().map(|(arg, value)| (env_name(arg), value.clone())).collect()
}

pub fn usage(task: &Task) -> String {
    let about = task.declaration.description.as_ref().map(|text| format!(" — {text}")).unwrap_or_default();
    let mut text = format!("{}{about}\n\nusage: caim {}", task.name, task.name);
    for arg in &task.declaration.args {
        let shape = if arg.flag { format!("--{}", arg.name) } else { format!("--{} <value>", arg.name) };
        text += &if arg.required { format!(" {shape}") } else { format!(" [{shape}]") };
    }
    text.push('\n');
    if !task.declaration.args.is_empty() {
        text += "\n";
        for arg in &task.declaration.args {
            let mut notes = Vec::new();
            if !arg.choices.is_empty() {
                notes.push(format!("one of {}", arg.choices.join(", ")));
            }
            if let Some(default) = &arg.default {
                notes.push(format!("default {default}"));
            }
            if arg.required {
                notes.push("required".into());
            }
            let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
            text += &format!("  --{:<18} {}{notes}  [{}]\n", arg.name, arg.help, env_name(&arg.name));
        }
    }
    let mut facts = vec![format!("declared in {}", task.source)];
    if let Some(group) = &task.declaration.group {
        facts.push(format!("group {group}"));
    }
    if !task.declaration.requires.is_empty() {
        facts.push(format!("runs {} first", task.declaration.requires.join(", ")));
    }
    if task.declaration.human {
        facts.push("run by a person, not an agent".into());
    }
    if !task.declaration.guard {
        facts.push("no stall guard".into());
    }
    for group in &task.declaration.concurrency {
        let mode = match group.mode {
            concurrency::Mode::Exclusive => "alone".to_string(),
            concurrency::Mode::Shared => match group.limit {
                Some(limit) => format!("shared, at most {limit} at once"),
                None => "shared".into(),
            },
        };
        facts.push(format!("holds {} ({mode}, per {})", group.group, group.scope.name()));
    }
    text + &format!("\n{}\n", facts.join("; "))
}

fn list(tasks: &BTreeMap<String, Task>) -> i32 {
    let shown: Vec<&Task> = tasks.values().filter(|task| !task.declaration.hidden).collect();
    let mut groups: BTreeMap<Option<&str>, Vec<&Task>> = BTreeMap::new();
    for task in shown {
        groups.entry(task.declaration.group.as_deref()).or_default().push(task);
    }
    for (group, in_group) in groups {
        if let Some(group) = group {
            println!("{group}:");
        }
        for task in in_group {
            let human = if task.declaration.human { " (a person runs it)" } else { "" };
            let description = task.declaration.description.as_deref().unwrap_or_default();
            println!("  {:<22} {description}{human}", task.name);
        }
    }
    OK
}

fn general_usage() -> &'static str {
    "usage: caim list | caim [--help] <task> [--name value ...] [<task> [--name value ...] ...]"
}

pub const PROJECT_FILE: &str = "caim.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    name: String,
    #[serde(default)]
    hooks: BTreeMap<String, Hook>,
    #[serde(default)]
    tasks: BTreeMap<String, Declaration>,
}

fn project() -> Result<(PathBuf, ProjectFile)> {
    let root = match std::env::var_os("CAIM_ROOT") {
        Some(root) => PathBuf::from(root),
        None => {
            let mut directory = std::env::current_dir().map_err(|error| Error::io("current directory", error))?;
            loop {
                if directory.join(PROJECT_FILE).is_file() {
                    break directory;
                }
                if !directory.pop() {
                    return Err(setup(format!("no {PROJECT_FILE} in the current directory or above it")));
                }
            }
        }
    };
    let path = root.join(PROJECT_FILE);
    let text = fs::read_to_string(&path).map_err(|error| Error::io(path.display(), error))?;
    let file: ProjectFile = toml::from_str(&text).map_err(|error| setup(format!("{}: {error}", path.display())))?;
    if !valid_name(&file.name) {
        return Err(setup(format!(
            "{}: name {:?} is not lowercase letters, digits and dashes",
            path.display(),
            file.name
        )));
    }
    Ok((root, file))
}

pub fn load() -> Result<BTreeMap<String, Task>> {
    let (root, file) = project()?;
    discover(&root, file.hooks, file.tasks).map(|(_, tasks)| tasks)
}

pub fn main(args: &[String]) -> i32 {
    let found = project().and_then(|(root, file)| {
        discover(&root, file.hooks, file.tasks).map(|(hooks, tasks)| (root, file.name, hooks, tasks))
    });
    let (root, project, hooks, tasks) = match found {
        Ok(found) => found,
        Err(error) => {
            eprintln!("caim: {error}");
            return CONFIG;
        }
    };
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") | Some("help") => {
            println!("{}\n", general_usage());
            list(&tasks)
        }
        Some("list") => match &args[1..] {
            [] => list(&tasks),
            _ => {
                eprintln!("{}", general_usage());
                USAGE
            }
        },
        Some(_) => {
            let named = match name_tasks(&tasks, args) {
                Ok(named) if !named.tasks.is_empty() => named,
                Ok(_) => {
                    eprintln!("{}", general_usage());
                    return USAGE;
                }
                Err(message) => {
                    eprintln!("{message}");
                    return USAGE;
                }
            };
            if named.help {
                let texts: Vec<String> = named.tasks.iter().map(|(task, _)| usage(task)).collect();
                print!("{}", texts.join("\n"));
                return OK;
            }
            let mut runs = Vec::new();
            for (task, options) in &named.tasks {
                match parse_args(task, options) {
                    Ok(values) => runs.push((*task, values)),
                    Err(message) => {
                        eprintln!("{}: {message}\n\n{}", task.name, usage(task));
                        return USAGE;
                    }
                }
            }
            let mut runner = Runner::new(root, project, hooks, &tasks);
            for (task, values) in runs {
                if runner.done.contains(&task.name) {
                    continue;
                }
                let status = runner.run(task, &values);
                if status != OK {
                    return status;
                }
            }
            OK
        }
    }
}

enum Reason {
    Exited,
    Stalled,
    Interrupted,
    Unavailable,
    NotStarted,
    Required(String),
    Lock(String),
    Busy(String),
}

impl Reason {
    fn code(&self) -> &'static str {
        match self {
            Reason::Exited => "exited",
            Reason::Stalled => "stalled",
            Reason::Interrupted => "interrupted",
            Reason::Unavailable => "unavailable",
            Reason::NotStarted => "not-started",
            Reason::Required(_) => "required",
            Reason::Lock(_) => "lock",
            Reason::Busy(..) => "busy",
        }
    }

    fn note(&self) -> String {
        match self {
            Reason::Exited => String::new(),
            Reason::NotStarted => " (did not start)".into(),
            Reason::Required(task) => format!(" (required task {task} did not pass)"),
            Reason::Lock(hook) => format!(" ({hook} hook)"),
            Reason::Busy(group) => format!(" (busy: {group})"),
            other => format!(" ({})", other.code()),
        }
    }
}

/// Writes to stdout never end the run: a reader that went away costs the output, not the
/// supervision.
struct Out {
    ended_line: bool,
}

impl Out {
    fn write(&self, bytes: &[u8]) {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(bytes);
        let _ = stdout.flush();
    }

    fn output(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.write(bytes);
        self.ended_line = bytes.ends_with(b"\n");
    }

    fn end_output(&mut self) {
        if !self.ended_line {
            self.write(b"\n");
        }
        self.ended_line = true;
    }

    fn message(&self, text: &str) {
        eprintln!("{text}");
    }

    fn messages(&self, bytes: &[u8]) {
        for line in String::from_utf8_lossy(bytes).lines() {
            self.message(line);
        }
    }

    fn waiting(&self, task: &str, group: &Concurrency, holders: &[Holder]) {
        eprintln!("{task} waits: {} is held by {}", group.group, concurrency::describe(holders));
    }

    fn hook(&self, line: &str) {
        self.write(format!("{line}\n").as_bytes());
    }

    fn ended(&self, task: &str, status: i32, reason: &Reason) {
        self.write(format!("[{task}] exit={status}{}\n", reason.note()).as_bytes());
    }
}

struct Channel {
    directory: PathBuf,
}

impl Channel {
    fn new() -> Result<Self> {
        let template = CString::new(std::env::temp_dir().join("caim.XXXXXX").to_string_lossy().into_owned())
            .map_err(|_| setup("the temporary directory's path holds a NUL byte"))?;
        let mut bytes = template.into_bytes_with_nul();
        if unsafe { libc::mkdtemp(bytes.as_mut_ptr() as *mut libc::c_char) }.is_null() {
            return Err(Error::os("create the run's temporary directory"));
        }
        bytes.pop();
        let directory = PathBuf::from(String::from_utf8_lossy(&bytes).into_owned());
        let channel = Channel { directory };
        for file in ["watch", "launches", "started"] {
            fs::write(channel.path(file), "").map_err(|error| Error::io(format!("write {file}"), error))?;
        }
        fs::create_dir(channel.path("watchers")).map_err(|error| Error::io("create watchers", error))?;
        Ok(channel)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    fn text(&self, name: &str) -> String {
        self.path(name).to_string_lossy().into_owned()
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn ps(pid: i32, field: &str) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", &format!("{field}=")])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string())
}

struct Runner<'a> {
    root: PathBuf,
    project: String,
    hooks: Commands,
    tasks: &'a BTreeMap<String, Task>,
    done: BTreeSet<String>,
    out: RefCell<Out>,
    held: Vec<String>,
}

impl<'a> Runner<'a> {
    fn new(root: PathBuf, project: String, hooks: Commands, tasks: &'a BTreeMap<String, Task>) -> Self {
        let held = std::env::var(concurrency::HELD)
            .map(|text| text.lines().filter(|line| !line.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        Runner { root, project, hooks, tasks, done: BTreeSet::new(), out: RefCell::new(Out { ended_line: true }), held }
    }

    fn set_held(&self) {
        // The runner is single-threaded; what it starts inherits this environment.
        unsafe { std::env::set_var(concurrency::HELD, self.held.join("\n")) };
    }

    fn directory(&self, task: &Task) -> PathBuf {
        task.declaration.dir.as_ref().map_or_else(|| self.root.clone(), |dir| self.root.join(dir))
    }

    fn hook(&self, name: &str) -> Command {
        let command = &self.hooks[name];
        let mut hook = Command::new(&command[0]);
        hook.args(&command[1..]);
        hook
    }

    fn call_hook(&self, hook: &str, command: &mut Command) -> i32 {
        let output = command.current_dir(&self.root).stdin(Stdio::null()).stderr(Stdio::inherit()).output();
        let out = self.out.borrow();
        match output {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    out.hook(line);
                }
                output.status.code().unwrap_or_else(|| {
                    let signal = std::os::unix::process::ExitStatusExt::signal(&output.status).unwrap_or(0);
                    out.message(&format!("the {hook} hook was killed by signal {signal}"));
                    128 + signal
                })
            }
            Err(error) => {
                out.message(&format!("the {hook} hook could not run: {error}"));
                error.raw_os_error().map_or(FAILED, not_started)
            }
        }
    }

    fn run(&mut self, task: &Task, values: &BTreeMap<String, String>) -> i32 {
        let name = &task.name;
        for required in &task.declaration.requires {
            if self.done.contains(required) {
                continue;
            }
            let other = &self.tasks[required];
            let defaults = parse_args(other, &[]).unwrap_or_default();
            let status = self.run(other, &defaults);
            if status != OK {
                self.out.borrow().ended(name, status, &Reason::Required(required.clone()));
                return status;
            }
        }
        self.done.insert(name.clone());

        let acquired = concurrency::acquire(
            &task.declaration.concurrency,
            name,
            &self.project,
            &self.root,
            &self.held,
            &mut |group, holders| self.out.borrow().waiting(name, group, holders),
        );
        let held = match acquired {
            Ok(Outcome::Held(held)) => held,
            Ok(Outcome::Busy { group, holders }) => {
                let out = self.out.borrow();
                out.message(&format!(
                    "{name} did not start; {} is held by {}",
                    group.group,
                    concurrency::describe(&holders)
                ));
                out.ended(name, BUSY, &Reason::Busy(group.group));
                return BUSY;
            }
            Err(Error::Interrupted(signal)) => {
                let out = self.out.borrow();
                out.message(&format!("{name} was interrupted while it waited for its concurrency groups"));
                out.ended(name, 128 + signal, &Reason::Interrupted);
                return 128 + signal;
            }
            Err(error) => {
                let out = self.out.borrow();
                out.message(&format!("{name} did not start; {error}"));
                out.ended(name, FAILED, &Reason::NotStarted);
                return FAILED;
            }
        };
        let before = self.held.len();
        self.held.extend(held.iter().map(|group| group.key.clone()));
        self.set_held();
        let status = self.run_holding(task, values);
        drop(held);
        self.held.truncate(before);
        self.set_held();
        status
    }

    fn run_holding(&mut self, task: &Task, values: &BTreeMap<String, String>) -> i32 {
        let name = &task.name;
        if let Some(lock) = &task.declaration.lock {
            let status = self.call_hook(lock, self.hook(lock).env("CAIM_TASK", name).envs(arguments(values)));
            if status != OK {
                self.out.borrow().ended(name, status, &Reason::Lock(lock.clone()));
                return status;
            }
        }

        let missing = match (missing_program(task), &task.declaration.dir) {
            (Some(program), _) if Path::new(&task.command[0]).exists() => {
                Some((CANNOT_EXECUTE, format!("{program} is not executable")))
            }
            (Some(program), _) => Some((NOT_FOUND, format!("{program} does not exist"))),
            (None, Some(dir)) if !self.directory(task).is_dir() => {
                Some((CONFIG, format!("its dir {dir} is not a directory")))
            }
            _ => None,
        };
        if let Some((status, missing)) = missing {
            let out = self.out.borrow();
            out.message(&format!("{name} did not start; {missing}"));
            out.ended(name, status, &Reason::NotStarted);
            return status;
        }
        let channel = match Channel::new() {
            Ok(channel) => channel,
            Err(error) => {
                let out = self.out.borrow();
                out.message(&format!("{name} did not start; {error}"));
                out.ended(name, FAILED, &Reason::NotStarted);
                return FAILED;
            }
        };
        let (status, reason) = self.supervised(task, values, &channel);
        let status = self.after(task, values, &channel, status);
        self.out.borrow().ended(name, status, &reason);
        status
    }

    fn supervised(&self, task: &Task, values: &BTreeMap<String, String>, channel: &Channel) -> (i32, Reason) {
        let name = &task.name;
        let say = |text: &str| self.out.borrow().message(text);
        let idle = match (task.declaration.idle, std::env::var("CAIM_STALL_SECONDS")) {
            (Some(idle), _) => idle,
            (None, Ok(text)) => match text.parse::<f64>() {
                Ok(seconds) if seconds.is_finite() && seconds > 0.0 => seconds,
                _ => {
                    say(&format!("{name} did not start; CAIM_STALL_SECONDS={text:?} is not a positive number"));
                    return (CONFIG, Reason::NotStarted);
                }
            },
            (None, Err(_)) => crate::progress::STALL_SECONDS,
        };
        let declared = channel.text("watch");
        let registry = channel.text("launches");
        let watchers = channel.text("watchers");
        // A runner started under another guard registers with it; the watchers started under this
        // run register here.
        let parent_watchers = std::env::var("CAIM_WATCHERS").unwrap_or_default();
        let mut environment: Vec<(String, String)> = vec![
            ("CAIM_DECLARED".into(), declared.clone()),
            ("CAIM_LAUNCH_REGISTRY".into(), registry.clone()),
            ("CAIM_WATCHERS".into(), watchers.clone()),
            ("CAIM_STALL_SECONDS".into(), idle.to_string()),
            ("CAIM_TASK".into(), name.clone()),
        ];
        environment.extend(arguments(values));
        environment.extend(task.declaration.env.iter().map(|(key, value)| (key.clone(), value.clone())));
        let guarded = task.declaration.guard;
        let at = |path: &String| self.root.join(path).to_string_lossy().into_owned();
        let watch = &task.declaration.watch;
        let plan = RunPlan {
            command: task.command.clone(),
            registration: if guarded { parent_watchers } else { String::new() },
            defer: watchers,
            idle: guarded.then_some(idle),
            watch: if guarded {
                WatchPlan {
                    logs: watch.logs.iter().map(at).collect(),
                    directories: watch.dirs.iter().map(at).collect(),
                    declarations: declared,
                }
            } else {
                WatchPlan::default()
            },
            steps: stop_steps(task),
            directory: Some(self.directory(task).to_string_lossy().into_owned()),
            inherit: !guarded,
            capture: guarded,
            environment,
            ..RunPlan::default()
        };
        let mut stall = String::new();
        let mut broken = String::new();
        let mut stops: Vec<u8> = Vec::new();
        let ended = native::supervise(
            &plan,
            Report {
                started: &mut |_| {},
                stalled: &mut |pid| {
                    stall = format!(
                        "{name} (pid {pid}) made no progress for {idle} s: no new kind of line in its output or declared logs and no new entry in its declared directories"
                    )
                },
                unavailable: &mut |error| broken = error.to_string(),
                stops: &mut stops,
                output: &mut |bytes| self.out.borrow_mut().output(bytes),
            },
        );
        self.out.borrow_mut().end_output();
        native::restore_signals();
        let show_stops = |stops: &[u8]| {
            self.out.borrow().messages(stops);
            let mut launched = Vec::new();
            self.stop_launches(&registry, &plan.steps, &mut launched);
            self.out.borrow().messages(&launched);
        };
        match ended {
            Ok(Ended::Exit(status)) => (status, Reason::Exited),
            Ok(Ended::Stall(_)) => {
                say(&stall);
                show_stops(&stops);
                (STALLED, Reason::Stalled)
            }
            Ok(Ended::Interrupted(signal, _)) => {
                show_stops(&stops);
                say(&format!("{name} was interrupted by signal {signal}"));
                (128 + signal, Reason::Interrupted)
            }
            Ok(Ended::Unavailable(message, _)) => {
                show_stops(&stops);
                say(&format!("{name} was stopped: its progress could not be watched ({message})"));
                (FAILED, Reason::Unavailable)
            }
            Err(error) => {
                say(&format!("{name} did not start: {}{error}", if broken.is_empty() { "" } else { "watch broke: " }));
                let status = match error {
                    Error::Start(errno, _) => not_started(errno),
                    _ => FAILED,
                };
                (status, Reason::NotStarted)
            }
        }
    }

    fn stop_launches(&self, registry: &str, steps: &[StopStep], report: &mut Vec<u8>) {
        let Ok(text) = fs::read_to_string(registry) else { return };
        let mut owned = Vec::new();
        for line in text.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(pid), Some(started), Some(pattern)) = (fields.next(), fields.next(), fields.next()) else {
                continue;
            };
            let Ok(pid) = pid.parse::<i32>() else { continue };
            if pid <= 0 || started.is_empty() || pattern.is_empty() {
                continue;
            }
            let Ok(pattern) = Regex::new(pattern) else { continue };
            let current = ps(pid, "lstart");
            if current.as_deref().map(str::trim) != Some(started.trim()) {
                continue;
            }
            if ps(pid, "command").is_some_and(|command| pattern.is_match(&command)) {
                owned.push(pid);
            }
        }
        if !owned.is_empty() {
            let _ = native::stop(0, &owned, steps, report);
        }
    }

    fn after(&self, task: &Task, values: &BTreeMap<String, String>, channel: &Channel, mut status: i32) -> i32 {
        for hook in &task.declaration.after {
            let code = self.call_hook(
                hook,
                self.hook(hook)
                    .env("CAIM_TASK", &task.name)
                    .envs(arguments(values))
                    .env("CAIM_STATUS", status.to_string())
                    .env("CAIM_STARTED", channel.path("started")),
            );
            if code != OK {
                status = code;
            }
        }
        status
    }
}
