mod events;
mod process;
mod sources;
mod watch;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(not(target_os = "macos"))]
compile_error!("caim has no src/native implementation for this operating system");

pub use events::{exited_pid, monotonic, process_exited, restore_signals, Event, Interest, Token, TERMINATION};
pub use os::{Queue, Watch};
pub use process::{
    reap, record, run, signal_name, signal_number, spawn, stop, supervise, Ended, Report, RunPlan, StopStep,
};
pub use sources::{PosixFiles, Sources};
pub use watch::{
    activity, judging_elsewhere, ready_file, stall_message, watch, Registration, WatchPlan, Watched, READ_BUDGET,
};
