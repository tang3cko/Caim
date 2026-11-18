use super::Command;
use crate::error::OK;

pub const COMMAND: Command = Command {
    name: "version",
    aliases: &["--version", "-V"],
    about: "print caim's version",
    usage: "usage: caim version\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    if args.len() != 1 {
        return super::usage_error(&COMMAND);
    }
    println!("caim {}", env!("CARGO_PKG_VERSION"));
    OK
}
