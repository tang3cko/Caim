mod activity;
mod completions;
mod exit;
mod first_exit;
mod hash;
mod hash_check;
mod help;
mod init;
mod line;
mod list;
mod run;
mod script;
mod stop;
mod version;
mod written;

use crate::error::{Error, FAILED, OK, Result, USAGE};

pub type Exec = fn(&[String]) -> i32;

pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub about: &'static str,
    pub usage: &'static str,
    pub exec: Exec,
}

pub const BUILTIN: &[Command] = &[
    init::COMMAND,
    list::COMMAND,
    help::COMMAND,
    version::COMMAND,
    script::COMMAND,
    completions::COMMAND,
    hash::COMMAND,
    hash_check::COMMAND,
    exit::COMMAND,
    first_exit::COMMAND,
    written::COMMAND,
    line::COMMAND,
    activity::COMMAND,
    run::COMMAND,
    stop::COMMAND,
];

const HELP_FLAGS: &[&str] = &["--help", "-h"];

pub fn find(word: &str) -> Option<&'static Command> {
    BUILTIN.iter().find(|command| command.name == word || command.aliases.contains(&word))
}

/// A task cannot take a name that runs a built-in command, as a cargo alias cannot shadow one.
pub fn reserved(name: &str) -> bool {
    find(name).is_some()
}

pub fn general_usage() -> String {
    let mut text = String::from(
        "usage: caim [--dry-run] <task> [--name value ...] [<task> [--name value ...] ...]\n       caim <task> --help\n       caim <command> [...]\n\ncommands:\n",
    );
    for command in BUILTIN {
        text += &format!("  {:<14} {}\n", command.name, command.about);
    }
    text += "\n";
    for command in BUILTIN {
        for line in command.usage.lines().take_while(|line| !line.is_empty()) {
            text += &format!("  {}\n", line.trim_start_matches("usage: ").trim_start());
        }
    }
    text
}

pub fn main(args: &[String]) -> i32 {
    let Some(first) = args.first() else { return (help::COMMAND.exec)(&["help".into()]) };
    match find(first) {
        Some(command) if args.len() == 2 && HELP_FLAGS.contains(&args[1].as_str()) => {
            print!("{}", command.usage);
            OK
        }
        Some(command) => (command.exec)(args),
        None => crate::tasks::main(args),
    }
}

fn usage_error(command: &Command) -> i32 {
    eprint!("{}", command.usage.lines().next().map(|line| format!("{line}\n")).unwrap_or_default());
    USAGE
}

fn wait(args: &[String]) -> i32 {
    report(args, "the watch failed: ", crate::waits::dispatch(args))
}

fn report(args: &[String], failure: &str, result: Result<i32>) -> i32 {
    match result {
        Ok(status) => status,
        Err(Error::Usage(message)) => {
            eprintln!("caim {}: {message}", args[0]);
            USAGE
        }
        Err(error) => {
            eprintln!("caim {}: {failure}{error}", args[0]);
            FAILED
        }
    }
}
