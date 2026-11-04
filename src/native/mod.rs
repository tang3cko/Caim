mod events;
mod process;
mod sources;
mod watch;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("caim has no src/native implementation for this operating system");

pub use events::{Event, Interest, TERMINATION, Token, exited_pid, monotonic, process_exited, restore_signals};
pub use os::{Queue, Watch};
pub use process::{
    Ended, Report, RunPlan, StopStep, reap, record, run, signal_name, signal_number, spawn, stop, supervise,
};
pub use sources::{PosixFiles, Sources};
pub use watch::{
    READ_BUDGET, Registration, WatchPlan, Watched, activity, judging_elsewhere, ready_file, stall_message, watch,
};
