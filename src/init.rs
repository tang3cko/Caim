use crate::error::{CANT_CREATE, FAILED, OK, USAGE};
use crate::tasks::{PROJECT_FILE, valid_name};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

pub const FLAG: &str = "--init";

// Lines of the reference are commented with "# ", so removing that prefix gives a valid caim.toml; prose is
// commented with "##". tests/init.rs holds the file to that.
const TEMPLATE: &str = r#"## The tasks of this project: `caim list` shows them, `caim <task> --help` explains one.
## The project's name; checkouts that share it share their concurrency groups.
name = "{name}"

[tasks.hello]
description = "Say hello"
run = "echo hello"

## Every key a task can take. Only run is required.
##
# [tasks.build]
# description = "Build"                  # one line for list and --help
# group = "make"                         # the heading list shows the task under
# run = ["sh", "-c", "echo building $CAIM_ARG_PROFILE"]  # a list runs with no shell; a string goes through sh -c
# requires = ["hello"]                   # run first, each to success; { task = "hello", args = { ... } } gives arguments
# parallel = false                       # run requires at once
# guard = true                           # stop the run when it makes no progress; false gives it the terminal
# idle = 180                             # seconds without progress before it is stopped
# stop = ["TERM:20", "KILL:20"]          # the stop steps
# watch.logs = ["build.log"]             # progress besides the output
# watch.dirs = ["out"]
# env = { RUST_LOG = "info" }            # for this task's command alone
# dir = "."                              # where the command runs, inside the project
# lock = "ready"                         # a hook that must succeed first
# after = ["report"]                     # hooks that may change the status
# concurrency = "build"                  # or { group, mode, limit, scope, on_busy }
# human = false                          # a task a person runs rather than an agent
# hidden = false                         # left out of list
#
# [[tasks.build.args]]                   # seen by the command as CAIM_ARG_PROFILE
# name = "profile"
# help = "the build profile"
# choices = ["debug", "release"]         # or required = true, or flag = true
# default = "debug"
#
# [hooks.ready]
# run = "true"
#
# [hooks.report]
# run = "exit $CAIM_STATUS"
"#;

pub fn dispatch(args: &[String]) -> i32 {
    if args.len() != 1 {
        eprintln!("usage: caim {FLAG}");
        return USAGE;
    }
    let directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("caim: current directory: {error}");
            return FAILED;
        }
    };
    let path = directory.join(PROJECT_FILE);
    let text = TEMPLATE.replace("{name}", &name_for(&directory));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => match file.write_all(text.as_bytes()) {
            Ok(()) => {
                eprintln!("caim: wrote {}", path.display());
                OK
            }
            Err(error) => {
                eprintln!("caim: {}: {error}", path.display());
                CANT_CREATE
            }
        },
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            eprintln!("caim: {} already exists", path.display());
            CANT_CREATE
        }
        Err(error) => {
            eprintln!("caim: {}: {error}", path.display());
            CANT_CREATE
        }
    }
}

fn name_for(directory: &Path) -> String {
    let raw = directory.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut name = String::new();
    for c in raw.chars() {
        if c.is_ascii_lowercase() || (c.is_ascii_digit() && !name.is_empty()) {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-').to_string();
    if valid_name(&name) { name } else { "project".into() }
}
