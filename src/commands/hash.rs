use super::Command;

pub const COMMAND: Command = Command {
    name: "hash",
    aliases: &[],
    about: "print sha256 lines for NUL-separated paths on stdin",
    usage: "usage: caim hash < NUL-separated paths\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    super::report(args, "", crate::digests::hash(&args[1..]))
}
