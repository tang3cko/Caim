use super::Command;

pub const COMMAND: Command = Command {
    name: "activity",
    aliases: &[],
    about: "watch a process caim did not start, by its progress",
    usage: "usage: caim activity --pid PID --idle SECONDS [--log FILE] [--dir DIR] [--declared FILE]\n",
    exec: super::wait,
};
