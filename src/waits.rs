use crate::error::{Error, Result, PROCESS_GONE, VOID};
use crate::native::{self, monotonic, Interest, Queue, RunPlan, StopStep, WatchPlan};
use regex::bytes::Regex;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const MODES: &[&str] = &["exit", "first-exit", "written", "line", "activity", "run", "stop"];

fn seconds(text: &str, positive: bool) -> Result<f64> {
    let invalid = || Error::Usage(format!("invalid seconds: {text}"));
    let value: f64 = text.parse().map_err(|_| invalid())?;
    if !value.is_finite() || value < 0.0 || (positive && value == 0.0) || value > i32::MAX as f64 {
        return Err(invalid());
    }
    Ok(value)
}

fn process_id(text: &str) -> Result<i32> {
    let value: i64 = text.parse().map_err(|_| Error::Usage(format!("invalid PID: {text}")))?;
    if value <= 0 || value > i32::MAX as i64 {
        return Err(Error::Usage("PID must be positive".into()));
    }
    Ok(value as i32)
}

fn timeout(message: &str) -> i32 {
    eprintln!("VOID: {message} when its hang guard ran out");
    VOID
}

fn wait_exit(pids: &[i32], guard: Option<f64>, first: bool) -> Result<i32> {
    let queue = Queue::new()?;
    for &pid in pids {
        if !queue.process(pid)? {
            if first {
                println!("exit {pid}");
            }
            return Ok(0);
        }
    }
    let events = queue.wait(guard)?;
    let Some(pid) = events.iter().find_map(native::exited_pid) else { return Ok(timeout("no process exited")) };
    if first {
        println!("exit {pid}");
    }
    Ok(0)
}

fn wait_written(path: &str, guard: Option<f64>) -> Result<i32> {
    let file = File::open(path).map_err(|error| Error::io(format!("open {path}"), error))?;
    let queue = Queue::new()?;
    let _watch = queue
        .watch(Path::new(path), true, Interest::Written)
        .map_err(|error| Error::io(format!("open {path}"), error))?;
    loop {
        let size = file.metadata().map_err(|error| Error::io(format!("fstat {path}"), error))?.len();
        if size > 0 {
            return Ok(0);
        }
        if queue.wait(guard)?.is_empty() {
            return Ok(timeout(&format!("{path} was still empty")));
        }
    }
}

struct LineWatch {
    path: String,
    file: File,
    pattern: Regex,
    gate: Option<Regex>,
    gated: bool,
    pending: Vec<u8>,
}

impl LineWatch {
    fn new(path: &str, pattern: &str, gate: Option<&str>) -> Result<Self> {
        let compile =
            |text: &str| Regex::new(text).map_err(|error| Error::Usage(format!("invalid regular expression: {error}")));
        Ok(LineWatch {
            path: path.to_string(),
            file: File::open(path).map_err(|error| Error::io(format!("open {path}"), error))?,
            pattern: compile(pattern)?,
            gate: gate.map(compile).transpose()?,
            gated: gate.is_none(),
            pending: Vec::new(),
        })
    }

    fn matches(&mut self) -> Result<bool> {
        let mut bytes = [0u8; 65536];
        loop {
            let count = match self.file.read(&mut bytes) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::io("read watched line", error)),
            };
            if count == 0 {
                return Ok(false);
            }
            self.pending.extend_from_slice(&bytes[..count]);
            let mut begin = 0;
            while let Some(offset) = self.pending[begin..].iter().position(|&c| c == b'\n') {
                let line = &self.pending[begin..begin + offset];
                begin += offset + 1;
                if !self.gated {
                    self.gated = self.gate.as_ref().is_some_and(|gate| gate.is_match(line));
                } else if self.pattern.is_match(line) {
                    return Ok(true);
                }
            }
            self.pending.drain(..begin);
        }
    }
}

fn wait_line(mut watches: Vec<LineWatch>, pids: &[i32], guard: Option<f64>) -> Result<i32> {
    let queue = Queue::new()?;
    let mut watched = Vec::new();
    for watch in &watches {
        let path = &watch.path;
        watched.push(
            queue
                .watch(Path::new(path), true, Interest::Written)
                .map_err(|error| Error::io(format!("open {path}"), error))?,
        );
    }
    let mut gone = BTreeSet::new();
    for &pid in pids {
        if !queue.process(pid)? {
            gone.insert(pid);
        }
    }
    let deadline = guard.map(|guard| monotonic() + guard);
    loop {
        // A final line wins even when process exit arrived in the same batch.
        for (index, watch) in watches.iter_mut().enumerate() {
            if watch.matches()? {
                println!("match {index}");
                return Ok(0);
            }
        }
        if let Some(pid) = gone.first() {
            println!("exit {pid}");
            return Ok(PROCESS_GONE);
        }
        if deadline.is_some_and(|deadline| monotonic() >= deadline) {
            return Ok(timeout("no line matched"));
        }
        let remaining = deadline.map(|deadline| (deadline - monotonic()).max(0.0));
        for event in queue.wait(remaining)? {
            if let Some(pid) = native::exited_pid(&event) {
                gone.insert(pid);
            }
        }
    }
}

fn stop_step(text: &str) -> Result<StopStep> {
    let (name, grace) =
        text.split_once(':').ok_or_else(|| Error::Usage(format!("expected SIGNAL:GRACE_SECONDS, got {text}")))?;
    let signal = native::signal_number(name).ok_or_else(|| Error::Usage(format!("unknown signal: {name}")))?;
    Ok(StopStep { signal, grace: seconds(grace, false)? })
}

pub fn absolute(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(Error::Usage("empty path".into()));
    }
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    let current = std::env::current_dir().map_err(|error| Error::io("current directory", error))?;
    // Lexically normal, as the paths the scripts declare are compared as text.
    let mut parts: Vec<String> = Vec::new();
    for part in current.to_string_lossy().split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part.to_string()),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

pub fn dispatch(args: &[String]) -> Result<i32> {
    let mode = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Error::Usage("expected exit, first-exit, written, line, activity, run, or stop".into()))?;
    if mode == "exit" || mode == "written" {
        if args.len() < 2 || args.len() > 3 {
            return Err(Error::Usage("expected PID/FILE [GUARD_SECONDS]".into()));
        }
        let guard = args.get(2).map(|text| seconds(text, false)).transpose()?;
        return if mode == "exit" {
            wait_exit(&[process_id(&args[1])?], guard, false)
        } else {
            wait_written(&args[1], guard)
        };
    }
    if !MODES.contains(&mode) {
        return Err(Error::Usage(format!("unknown mode: {mode}")));
    }
    let watching = mode == "activity" || mode == "run";
    let stopping = mode == "stop" || mode == "run";
    let mut pids = Vec::new();
    let mut plan = WatchPlan::default();
    let mut launch = RunPlan::default();
    let mut matches = Vec::new();
    let mut after = Vec::new();
    let mut steps = Vec::new();
    let mut guard = None;
    let mut idle = None;
    let mut group = None;
    let (mut ready, mut registration, mut defer) = (String::new(), String::new(), String::new());
    let mut i = 1;
    while i < args.len() {
        let option = args[i].as_str();
        let mut next = || -> Result<String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| Error::Usage(format!("missing value for {option}")))
        };
        match option {
            "--" if mode == "run" => {
                launch.command = args[i + 1..].to_vec();
                break;
            }
            "--pid" if mode != "run" => pids.push(process_id(&next()?)?),
            "--guard" if !watching && mode != "stop" => guard = Some(seconds(&next()?, false)?),
            "--idle" if watching => idle = Some(seconds(&next()?, true)?),
            "--log" if watching => plan.logs.push(absolute(&next()?)?),
            "--dir" if watching => plan.directories.push(absolute(&next()?)?),
            "--declared" if watching => plan.declarations = absolute(&next()?)?,
            "--output" if mode == "run" => launch.output = absolute(&next()?)?,
            "--outcome" if mode == "run" => launch.outcome = absolute(&next()?)?,
            "--group" if mode == "stop" => group = Some(process_id(&next()?)?),
            "--step" if stopping => steps.push(stop_step(&next()?)?),
            "--ready" if watching => ready = next()?,
            "--register" if watching => registration = next()?,
            "--defer" if watching => defer = next()?,
            "--match" if mode == "line" => {
                let path = next()?;
                let pattern = next()?;
                matches.push((path, pattern, None));
            }
            "--after" if mode == "line" => {
                let path = next()?;
                let gate = next()?;
                let pattern = next()?;
                after.push((path, pattern, Some(gate)));
            }
            _ => return Err(Error::Usage(format!("unknown option: {option}"))),
        }
        i += 1;
    }
    match mode {
        "activity" => {
            let Some(idle) =
                idle.filter(|_| pids.len() == 1 && (!plan.logs.is_empty() || !plan.declarations.is_empty()))
            else {
                return Err(Error::Usage("activity needs --pid, --idle, and --log or --declared".into()));
            };
            native::activity(pids[0], idle, &plan, &ready, &registration, &defer)
        }
        "run" => {
            if idle.is_none()
                || launch.command.is_empty()
                || launch.output.is_empty()
                || launch.outcome.is_empty()
                || steps.is_empty()
            {
                return Err(Error::Usage("run needs --idle, --output, --outcome, --step and -- COMMAND".into()));
            }
            launch.idle = idle;
            plan.logs.insert(0, launch.output.clone());
            launch.watch = plan;
            launch.ready = ready;
            launch.registration = registration;
            launch.defer = defer;
            launch.steps = steps;
            native::run(&launch)
        }
        "stop" => {
            if (group.is_none() && pids.is_empty()) || steps.is_empty() {
                return Err(Error::Usage("stop needs --group or --pid, and --step".into()));
            }
            native::stop(group.unwrap_or(0), &pids, &steps, &mut io::stdout())
        }
        "first-exit" => {
            if pids.iter().collect::<BTreeSet<_>>().len() < 2 {
                return Err(Error::Usage("first-exit needs two distinct PIDs".into()));
            }
            wait_exit(&pids, guard, true)
        }
        _ => {
            matches.extend(after);
            if matches.is_empty() {
                return Err(Error::Usage("line needs --match or --after".into()));
            }
            let watches = matches
                .iter()
                .map(|(path, pattern, gate)| LineWatch::new(path, pattern, gate.as_deref()))
                .collect::<Result<Vec<_>>>()?;
            wait_line(watches, &pids, guard)
        }
    }
}
