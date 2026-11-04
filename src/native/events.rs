use std::os::fd::RawFd;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interest {
    Written,
    Touched,
    Entries,
    Source,
    Ancestor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Exited(i32),
    Readable(RawFd),
    Changed(Token),
    Child,
    Suspend,
}

pub const TERMINATION: [i32; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

static BEFORE: Mutex<Vec<(i32, libc::sighandler_t)>> = Mutex::new(Vec::new());

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn ignore(signal: i32) {
    let mut before = BEFORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = unsafe { libc::signal(signal, libc::SIG_IGN) };
    if !before.iter().any(|(kept, _)| *kept == signal) {
        before.push((signal, previous));
    }
}

/// Puts back every signal a queue ignored as it was before: the default for a runner started
/// normally, still ignored for one started with it ignored (a background job).
pub fn restore_signals() {
    let mut before = BEFORE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (signal, handler) in before.drain(..) {
        unsafe { libc::signal(signal, handler) };
    }
    super::os::restore_platform();
}

pub fn exited_pid(event: &Event) -> Option<i32> {
    match event {
        Event::Exited(pid) => Some(*pid),
        _ => None,
    }
}

pub fn process_exited(events: &[Event]) -> bool {
    events.iter().any(|event| exited_pid(event).is_some())
}
