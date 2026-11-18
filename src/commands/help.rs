use super::Command;
use crate::error::OK;

pub const COMMAND: Command = Command {
    name: "help",
    aliases: &["--help", "-h"],
    about: "explain caim, a command or a task",
    usage: "usage: caim help [<command> | <task>]\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    match &args[1..] {
        [] => {
            print!("{}", super::general_usage());
            match crate::tasks::load() {
                Ok(tasks) => {
                    println!("\ntasks:");
                    crate::tasks::list(&tasks)
                }
                Err(error) => {
                    eprintln!("caim: {error}");
                    OK
                }
            }
        }
        [name] => match super::find(name) {
            Some(command) => {
                print!("{}", command.usage);
                OK
            }
            None => crate::tasks::main(&[name.clone(), "--help".into()]),
        },
        _ => super::usage_error(&COMMAND),
    }
}
