use super::Command;

pub const COMMAND: Command = Command {
    name: "init",
    aliases: &["--init"],
    about: "write a caim.toml to start from in the current directory",
    usage: "usage: caim init\n\nWrites a caim.toml named after the current directory, with one task and every key a task can take commented out.\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    if args.len() != 1 {
        return super::usage_error(&COMMAND);
    }
    crate::init::write()
}
