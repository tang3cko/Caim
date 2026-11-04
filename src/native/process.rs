use super::events::monotonic;
use super::os::{self, Queue};
use super::watch::{stall_message, watch_with, Registration, Stream, WatchPlan, Watched};
use crate::error::{Error, Result};
use crate::progress::Written;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

fn remaining(group: i32, pids: &[i32]) -> Result<BTreeSet<i32>> {
    let mut found = BTreeSet::new();
    if group > 0 {
        for member in os::group_members(group)? {
            if member > 0 && os::alive(member)? {
                found.insert(member);
            }
        }
    }
    for &pid in pids {
        if os::alive(pid)? {
            found.insert(pid);
        }
    }
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StopStep {
    pub signal: i32,
    pub grace: f64,
}

pub fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGINT => "INT".into(),
        libc::SIGTERM => "TERM".into(),
        libc::SIGKILL => "KILL".into(),
        libc::SIGHUP => "HUP".into(),
        libc::SIGQUIT => "QUIT".into(),
        other => other.to_string(),
    }
}

pub fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        _ => return None,
    })
}

fn listed(pids: &BTreeSet<i32>) -> String {
    pids.iter().map(i32::to_string).collect::<Vec<_>>().join(" ")
}

pub fn stop(group: i32, pids: &[i32], steps: &[StopStep], out: &mut dyn Write) -> Result<i32> {
    let mut left = remaining(group, pids)?;
    let send = |signal: i32, left: &BTreeSet<i32>| -> Result<()> {
        let check = |result: i32, what: &str| -> Result<()> {
            if result != 0 {
                let error = io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::EPERM)) {
                    return Err(Error::io(what, error));
                }
            }
            Ok(())
        };
        if group > 0 {
            check(unsafe { libc::kill(-group, signal) }, "signal process group")?;
        }
        for &pid in pids {
            if left.contains(&pid) {
                check(unsafe { libc::kill(pid, signal) }, "signal process")?;
            }
        }
        Ok(())
    };
    for step in steps {
        if left.is_empty() {
            return Ok(0);
        }
        send(step.signal, &left)?;
        // A stopped process cannot act on the signal until it runs again (as systemd and runsv do).
        if step.signal != libc::SIGKILL && step.signal != libc::SIGCONT {
            send(libc::SIGCONT, &left)?;
        }
        let _ = writeln!(out, "[stop] {} to {} process(es): {}", signal_name(step.signal), left.len(), listed(&left));
        let _ = out.flush();
        let queue = Queue::new()?;
        let mut watched = BTreeSet::new();
        let mut leaving = BTreeSet::new();
        let deadline = monotonic() + step.grace;
        loop {
            // Register first, then look, so no exit falls between; a process the kernel refuses is already leaving.
            for pid in remaining(group, pids)? {
                if watched.contains(&pid) || leaving.contains(&pid) {
                    continue;
                }
                if queue.process(pid)? {
                    watched.insert(pid);
                } else {
                    leaving.insert(pid);
                }
            }
            left = remaining(group, pids)?;
            left.retain(|pid| !leaving.contains(pid));
            if left.is_empty() {
                return Ok(0);
            }
            let time_left = deadline - monotonic();
            if time_left <= 0.0 {
                break;
            }
            queue.wait(Some(time_left))?;
        }
    }
    left = remaining(group, pids)?;
    if left.is_empty() {
        return Ok(0);
    }
    let last = steps.last().map_or("no signal".to_string(), |step| signal_name(step.signal));
    eprintln!("caim: process(es) {} outlived {last}", listed(&left));
    Ok(crate::error::STALLED)
}

#[derive(Clone, Debug, Default)]
pub struct RunPlan {
    pub command: Vec<String>,
    pub output: String,
    pub outcome: String,
    pub ready: String,
    pub registration: String,
    pub defer: String,
    pub idle: Option<f64>,
    pub watch: WatchPlan,
    pub steps: Vec<StopStep>,
    pub directory: Option<String>,
    pub inherit: bool,
    pub capture: bool,
    pub environment: Vec<(String, String)>,
}

fn environment(plan: &RunPlan) -> Result<Vec<CString>> {
    let mut entries: Vec<Vec<u8>> = std::env::vars_os()
        .filter(|(key, _)| !plan.environment.iter().any(|(name, _)| key.as_bytes() == name.as_bytes()))
        .map(|(key, value)| [key.as_bytes(), b"=", value.as_bytes()].concat())
        .collect();
    entries.extend(plan.environment.iter().map(|(name, value)| format!("{name}={value}").into_bytes()));
    entries
        .into_iter()
        .map(|entry| CString::new(entry).map_err(|_| Error::Usage("NUL byte in an environment variable".into())))
        .collect()
}

pub(super) fn cstring(text: &str) -> Result<CString> {
    CString::new(text).map_err(|_| Error::Usage(format!("NUL byte in {text:?}")))
}

fn output_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(Error::os("create the output pipe"));
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let set = unsafe {
        libc::fcntl(read.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == 0
            && libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == 0
            && libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) == 0
    };
    if !set {
        return Err(Error::os("prepare the output pipe"));
    }
    Ok((read, write))
}

pub fn spawn(plan: &RunPlan) -> Result<i32> {
    start(plan).map(|(pid, _)| pid)
}

fn start(plan: &RunPlan) -> Result<(i32, Option<OwnedFd>)> {
    let environment = environment(plan)?;
    let pipe = if plan.capture && !plan.inherit { Some(output_pipe()?) } else { None };
    let pid = os::start(plan, pipe.as_ref().map(|(_, write)| write.as_raw_fd()), &environment)?;
    Ok((pid, pipe.map(|(read, _)| read)))
}

pub fn reap(pid: i32, block: bool) -> Option<i32> {
    let mut status = 0;
    let reaped = loop {
        let reaped = unsafe { libc::waitpid(pid, &mut status, if block { 0 } else { libc::WNOHANG }) };
        if reaped < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        break reaped;
    };
    if reaped <= 0 {
        return None;
    }
    if libc::WIFEXITED(status) {
        return Some(libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        return Some(128 + libc::WTERMSIG(status));
    }
    None
}

pub fn record(path: &str, outcome: &str) {
    if !path.is_empty() {
        let _ = fs::write(path, format!("{outcome}\n"));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Exit(i32),
    Stall(Option<i32>),
    Interrupted(i32, Option<i32>),
    Unavailable(String, Option<i32>),
}

impl Ended {
    pub fn describe(&self) -> String {
        let reaped = |status: &Option<i32>| status.map_or(String::new(), |status| format!(" status={status}"));
        match self {
            Ended::Exit(status) => format!("exit {status}"),
            Ended::Stall(status) => format!("stall{}", reaped(status)),
            Ended::Interrupted(signal, status) => format!("interrupted {signal}{}", reaped(status)),
            Ended::Unavailable(_, status) => format!("unavailable{}", reaped(status)),
        }
    }
}

fn give_terminal(group: libc::pid_t) {
    // SAFETY: setting the foreground from the background raises SIGTTOU, which is ignored around the call.
    unsafe {
        let before = libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::tcsetpgrp(0, group);
        libc::signal(libc::SIGTTOU, before);
    }
}

fn terminal_modes() -> Option<libc::termios> {
    let mut modes: libc::termios = unsafe { std::mem::zeroed() };
    (unsafe { libc::tcgetattr(0, &mut modes) } == 0).then_some(modes)
}

fn set_terminal_modes(modes: &Option<libc::termios>) {
    if let Some(modes) = modes {
        let before = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        // TCSANOW: TCSADRAIN waits for output nobody may be reading while the job is stopped.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, modes) };
        unsafe { libc::signal(libc::SIGTTOU, before) };
    }
}

pub(super) struct Job {
    pid: i32,
    terminal: Option<(libc::pid_t, Option<libc::termios>)>,
    command_modes: Option<libc::termios>,
}

impl Job {
    fn new(pid: i32, inherit: bool) -> Self {
        let mut job = Job { pid, terminal: None, command_modes: None };
        if !inherit || unsafe { libc::isatty(0) } != 1 {
            return job;
        }
        let own = unsafe { libc::getpgrp() };
        if unsafe { libc::tcgetpgrp(0) } != own {
            return job;
        }
        let modes = terminal_modes();
        if unsafe { libc::tcsetpgrp(0, pid) } == 0 {
            job.terminal = Some((own, modes));
            // It may have read before it had the terminal, and been stopped for it.
            unsafe { libc::kill(-pid, libc::SIGCONT) };
        }
        job
    }

    pub(super) fn child_changed(&mut self) -> Option<f64> {
        let signal = os::stopped(self.pid)?;
        let holds = self.terminal.is_some() && unsafe { libc::tcgetpgrp(0) } == unsafe { libc::getpgrp() };
        if matches!(signal, libc::SIGTTIN | libc::SIGTTOU) && holds {
            give_terminal(self.pid);
            unsafe { libc::kill(-self.pid, libc::SIGCONT) };
            return None;
        }
        Some(self.suspend_self(signal))
    }

    pub(super) fn suspend(&mut self) -> f64 {
        unsafe { libc::kill(-self.pid, libc::SIGSTOP) };
        // Its stop is consumed here, so it is not later taken for a stop of its own.
        if os::stopped_or_exited(self.pid) {
            os::stopped(self.pid);
        }
        self.suspend_self(libc::SIGTSTP)
    }

    /// Stops with SIGNAL, as the shell expects of a job. In an orphaned process group, which no
    /// shell would continue, the kernel discards SIGTSTP, SIGTTIN and SIGTTOU and this goes on.
    fn suspend_self(&mut self, signal: i32) -> f64 {
        let began = monotonic();
        if let Some((own, modes)) = &self.terminal {
            self.command_modes = terminal_modes();
            give_terminal(*own);
            set_terminal_modes(modes);
        }
        // Where the queue takes signals blocked (Linux), it must be unblocked to stop this process.
        unsafe {
            let mut only: libc::sigset_t = std::mem::zeroed();
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut only);
            libc::sigaddset(&mut only, signal);
            let before = libc::signal(signal, libc::SIG_DFL);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, &mut mask);
            libc::kill(libc::getpid(), signal);
            libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            libc::signal(signal, before);
        }
        // Continued: the shell gave the terminal back when this runs in the foreground again.
        if self.terminal.is_some() && unsafe { libc::tcgetpgrp(0) } == unsafe { libc::getpgrp() } {
            set_terminal_modes(&self.command_modes);
            give_terminal(self.pid);
        }
        unsafe { libc::kill(-self.pid, libc::SIGCONT) };
        monotonic() - began
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // Only from the command: when the shell has moved the terminal elsewhere, it stays there.
        if let Some((own, modes)) = self.terminal.as_ref().filter(|_| unsafe { libc::tcgetpgrp(0) } == self.pid) {
            give_terminal(*own);
            set_terminal_modes(modes);
        }
    }
}

pub struct Report<'a> {
    pub started: &'a mut dyn FnMut(i32),
    pub stalled: &'a mut dyn FnMut(i32),
    pub unavailable: &'a mut dyn FnMut(&Error),
    pub stops: &'a mut dyn Write,
    pub output: &'a mut dyn FnMut(&[u8]),
}

pub fn supervise(plan: &RunPlan, report: Report<'_>) -> Result<Ended> {
    let Report { started: on_start, stalled: on_stall, unavailable: on_unavailable, stops, output } = report;
    let queue = Queue::new()?;
    queue.termination()?;
    queue.children()?;
    queue.suspension()?;
    let _registered = Registration::new(&plan.registration)?;
    let (pid, pipe) = start(plan)?;
    let mut job = Job::new(pid, plan.inherit);
    let mut stream = pipe.map(|fd| Stream { fd, sink: output, written: Written::stream(), open: true });
    on_start(pid);
    let end = |stops: &mut dyn Write| {
        // A leader the stop saw leave may still be exiting, so it is waited for; one that outlived every step is not.
        let left = matches!(stop(pid, &[], &plan.steps, stops), Ok(0));
        reap(pid, left)
    };
    let watched = match queue.process(pid) {
        Ok(true) => {
            watch_with(&queue, plan.idle, &plan.watch, &plan.ready, &plan.defer, stream.as_mut(), Some(&mut job))
        }
        Ok(false) => Ok(Watched::Exited),
        Err(error) => Err(error),
    };
    let ended = match watched {
        Ok(Watched::Exited) => match reap(pid, true) {
            // What the command left running in its group is left as it was, as timeout(1) does.
            Some(status) => Ended::Exit(status),
            None => Ended::Unavailable("the command could not be reaped".into(), None),
        },
        Ok(Watched::Stalled) => {
            on_stall(pid);
            Ended::Stall(end(stops))
        }
        Ok(Watched::Interrupted(signal)) => Ended::Interrupted(signal, end(stops)),
        Err(error) => {
            on_unavailable(&error);
            Ended::Unavailable(error.to_string(), end(stops))
        }
    };
    drop(job);
    if let Some(stream) = stream.as_mut() {
        stream.rest();
    }
    Ok(ended)
}

pub fn run(plan: &RunPlan) -> Result<i32> {
    let mut stdout = io::stdout();
    let idle = plan.idle.unwrap_or_default();
    let ended = supervise(
        plan,
        Report {
            started: &mut |pid| {
                println!("[run] pid {pid}");
                let _ = io::stdout().flush();
            },
            stalled: &mut |pid| eprintln!("{}", stall_message(pid, idle)),
            unavailable: &mut |error| eprintln!("caim: the watch failed: {error}"),
            stops: &mut stdout,
            output: &mut |_| {},
        },
    )?;
    record(&plan.outcome, &ended.describe());
    Ok(match ended {
        Ended::Exit(_) => 0,
        Ended::Stall(_) => crate::error::STALLED,
        Ended::Interrupted(signal, _) => 128 + signal,
        Ended::Unavailable(..) => crate::error::FAILED,
    })
}
