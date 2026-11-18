use super::Command;
use crate::error::CONFIG;

pub const COMMAND: Command = Command {
    name: "list",
    aliases: &[],
    about: "list the project's tasks",
    usage: "usage: caim list\n\nLists the tasks caim.toml declares, by group; it checks the whole file first.\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    if args.len() != 1 {
        return super::usage_error(&COMMAND);
    }
    match crate::tasks::load() {
        Ok(tasks) => crate::tasks::list(&tasks),
        Err(error) => {
            eprintln!("caim: {error}");
            CONFIG
        }
    }
}
