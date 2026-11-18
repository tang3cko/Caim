use super::Command;

pub const COMMAND: Command = Command {
    name: "line",
    aliases: &[],
    about: "wait for a line to match in a file",
    usage: "usage: caim line --match FILE REGEX [--after FILE GATE REGEX] [--pid PID] [--guard SECONDS]\n",
    exec: super::wait,
};
