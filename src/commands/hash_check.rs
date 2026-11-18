use super::Command;

pub const COMMAND: Command = Command {
    name: "hash-check",
    aliases: &[],
    about: "print the files that changed since lists of sha256 lines",
    usage: "usage: caim hash-check LIST...\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    super::report(args, "", crate::digests::hash_check(&args[1..]))
}
