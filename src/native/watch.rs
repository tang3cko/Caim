use super::events::{Event, Interest, monotonic};
use super::os::Queue;
use super::process::Job;
use super::sources::{PosixFiles, Sources};
use crate::error::{Error, Result};
use crate::progress::{FileSystem, StallClock, Written};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

pub const READ_BUDGET: u64 = 1 << 20;

pub fn judging_elsewhere(filesystem: &mut dyn FileSystem, directory: &str) -> Result<bool> {
    if directory.is_empty() {
        return Ok(false);
    }
    let Some(names) = filesystem.list(directory)? else { return Ok(false) };
    let own = std::process::id() as u64;
    for name in names {
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = name.parse::<u64>() else { continue };
        if pid == 0 || pid > i32::MAX as u64 || pid == own {
            continue;
        }
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EPERM) => return Ok(true),
            Some(libc::ESRCH) => {}
            _ => return Err(Error::io("check nested watcher", error)),
        }
    }
    Ok(false)
}

pub fn ready_file(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    fs::write(path, "watching\n").map_err(|error| Error::io(format!("cannot write readiness file {path}"), error))
}

pub fn stall_message(pid: i32, idle: f64) -> String {
    format!(
        "caim: no progress for {idle} s while process {pid} was running: no new kind of line in its declared logs and no new entry in its declared directories"
    )
}

pub struct Registration {
    path: Option<String>,
}

impl Registration {
    pub fn new(directory: &str) -> Result<Self> {
        if directory.is_empty() {
            return Ok(Registration { path: None });
        }
        fs::create_dir_all(directory).map_err(|error| Error::io(format!("create {directory}"), error))?;
        let path = format!("{directory}/{}", std::process::id());
        ready_file(&path)?;
        Ok(Registration { path: Some(path) })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct WatchPlan {
    pub logs: Vec<String>,
    pub directories: Vec<String>,
    pub declarations: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Watched {
    Exited,
    Stalled,
    Interrupted(i32),
}

pub(super) struct Stream<'a> {
    pub(super) fd: OwnedFd,
    pub(super) sink: &'a mut dyn FnMut(&[u8]),
    pub(super) written: Written,
    pub(super) open: bool,
}

impl Stream<'_> {
    fn read(&mut self, budget: u64) -> Result<(bool, bool)> {
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        let mut progress = false;
        while self.open && total < budget {
            let count =
                unsafe { libc::read(self.fd.as_raw_fd(), buffer.as_mut_ptr() as *mut libc::c_void, buffer.len()) };
            if count > 0 {
                let bytes = &buffer[..count as usize];
                (self.sink)(bytes);
                progress |= self.written.append(bytes);
                total += count as u64;
                continue;
            }
            if count == 0 {
                self.open = false;
                break;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok((progress, false)),
                _ => return Err(Error::io("read the command's output", error)),
            }
        }
        Ok((progress, self.open && total >= budget))
    }

    /// Only what is buffered now: something the command left running may hold the pipe open and
    /// keep writing.
    pub(super) fn rest(&mut self) {
        let mut available: libc::c_int = 0;
        if !self.open || unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::FIONREAD, &mut available) } < 0 {
            return;
        }
        let mut left = available.max(0) as usize;
        let mut buffer = [0u8; 64 * 1024];
        while left > 0 {
            let want = left.min(buffer.len());
            let count = unsafe { libc::read(self.fd.as_raw_fd(), buffer.as_mut_ptr() as *mut libc::c_void, want) };
            if count < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if count <= 0 {
                return;
            }
            (self.sink)(&buffer[..count as usize]);
            left -= count as usize;
        }
    }
}

pub fn watch(queue: &Queue, idle: Option<f64>, plan: &WatchPlan, ready: &str, defer: &str) -> Result<Watched> {
    watch_with(queue, idle, plan, ready, defer, None, None)
}

pub(super) fn watch_with(
    queue: &Queue,
    idle: Option<f64>,
    plan: &WatchPlan,
    ready: &str,
    defer: &str,
    stream: Option<&mut Stream<'_>>,
    job: Option<&mut Job>,
) -> Result<Watched> {
    match watch_inner(queue, idle, plan, ready, defer, stream, job) {
        Err(Error::Interrupted(signal)) => Ok(Watched::Interrupted(signal)),
        other => other,
    }
}

#[derive(Default)]
struct Pending {
    dirty: BTreeSet<usize>,
    handover: bool,
    stream: bool,
    child: bool,
    suspend: bool,
}

fn watch_inner(
    queue: &Queue,
    idle: Option<f64>,
    plan: &WatchPlan,
    ready: &str,
    defer: &str,
    mut stream: Option<&mut Stream<'_>>,
    mut job: Option<&mut Job>,
) -> Result<Watched> {
    let mut sources = Sources::new();
    for path in &plan.logs {
        sources.log(queue, path)?;
    }
    for path in &plan.directories {
        sources.directory(queue, path)?;
    }
    if !plan.declarations.is_empty() {
        sources.declarations(queue, &plan.declarations)?;
    }
    // A nearer watcher arriving or leaving is progress: the time it judged is not idle time here.
    let mut watchers = None;
    if !defer.is_empty() {
        fs::create_dir_all(defer).map_err(|error| Error::io(format!("create {defer}"), error))?;
        let watch = queue
            .watch(Path::new(defer), true, Interest::Entries)
            .map_err(|error| Error::io(format!("open {defer}"), error))?;
        watchers = Some(watch);
    }
    let watchers_token = watchers.as_ref().map(|watch| watch.token());
    let stream_fd = stream.as_ref().map(|stream| stream.fd.as_raw_fd());
    if let Some(fd) = stream_fd {
        queue.readable(fd)?;
    }
    let mut clock = match idle {
        Some(idle) => Some(StallClock::new(monotonic(), idle)?),
        None => None,
    };
    ready_file(ready)?;

    let mut pending = Pending::default();
    let absorb = |events: Vec<Event>, pending: &mut Pending, sources: &Sources| {
        for event in &events {
            match *event {
                Event::Exited(_) => return true,
                Event::Readable(fd) if Some(fd) == stream_fd => pending.stream = true,
                Event::Changed(token) if Some(token) == watchers_token => pending.handover = true,
                Event::Child => pending.child = true,
                Event::Suspend => pending.suspend = true,
                _ => {
                    if let Some(id) = sources.owner(event) {
                        pending.dirty.insert(id);
                    }
                }
            }
        }
        false
    };
    enum Outcome {
        Progress,
        Quiet,
        Exited,
    }
    let drain = |everything: bool,
                 pending: &mut Pending,
                 sources: &mut Sources,
                 stream: &mut Option<&mut Stream<'_>>|
     -> Result<Outcome> {
        let mut progress = std::mem::take(&mut pending.handover);
        if everything {
            pending.dirty.extend(0..sources.len());
            pending.stream |= stream.is_some();
        }
        loop {
            if pending.stream {
                let (step, more) = match stream.as_deref_mut() {
                    Some(stream) => stream.read(READ_BUDGET)?,
                    None => (false, false),
                };
                progress |= step;
                pending.stream = more;
            } else if let Some(&id) = pending.dirty.iter().next() {
                let (step, more) = sources.service(queue, id, READ_BUDGET)?;
                progress |= step;
                if !more {
                    pending.dirty.remove(&id);
                }
            } else {
                break;
            }
            if absorb(queue.wait(Some(0.0))?, pending, sources) {
                return Ok(Outcome::Exited);
            }
            progress |= std::mem::take(&mut pending.handover);
        }
        Ok(if progress { Outcome::Progress } else { Outcome::Quiet })
    };

    loop {
        let timeout = clock.as_ref().map(|clock| clock.remaining(monotonic()));
        let mut events = queue.wait(timeout)?;
        let mut resumed = false;
        loop {
            if absorb(events, &mut pending, &sources) {
                return Ok(Watched::Exited);
            }
            if resumed {
                // The SIGTSTP it sent itself to stop.
                pending.suspend = false;
            }
            let Some(job) = job.as_deref_mut() else { break };
            let suspended = if std::mem::take(&mut pending.suspend) {
                Some(job.suspend())
            } else if std::mem::take(&mut pending.child) {
                job.child_changed()
            } else {
                None
            };
            let Some(seconds) = suspended else { break };
            if let Some(clock) = clock.as_mut() {
                clock.pause(seconds);
            }
            resumed = true;
            events = queue.wait(Some(0.0))?;
        }
        let outcome = drain(false, &mut pending, &mut sources, &mut stream)?;
        let Some(clock) = clock.as_mut() else {
            if matches!(outcome, Outcome::Exited) {
                return Ok(Watched::Exited);
            }
            continue;
        };
        match outcome {
            Outcome::Exited => return Ok(Watched::Exited),
            outcome => {
                if !clock.observe(monotonic(), matches!(outcome, Outcome::Progress), false) {
                    continue;
                }
            }
        }
        // Before judging idle, read every source once more, in case an event was coalesced away.
        let outcome = drain(true, &mut pending, &mut sources, &mut stream)?;
        if matches!(outcome, Outcome::Exited) {
            return Ok(Watched::Exited);
        }
        let deferred = judging_elsewhere(&mut PosixFiles, defer)?;
        if !clock.observe(monotonic(), matches!(outcome, Outcome::Progress), deferred) {
            continue;
        }
        return Ok(Watched::Stalled);
    }
}

pub fn activity(pid: i32, idle: f64, plan: &WatchPlan, ready: &str, registration: &str, defer: &str) -> Result<i32> {
    let queue = Queue::new()?;
    queue.termination()?;
    let _registered = Registration::new(registration)?;
    if !queue.process(pid)? {
        return Ok(0);
    }
    match watch(&queue, Some(idle), plan, ready, defer)? {
        Watched::Exited => Ok(0),
        Watched::Stalled => {
            eprintln!("{}", stall_message(pid, idle));
            Ok(crate::error::STALLED)
        }
        Watched::Interrupted(signal) => Ok(128 + signal),
    }
}
