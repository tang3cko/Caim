use super::Command;

pub const COMMAND: Command = Command {
    name: "first-exit",
    aliases: &[],
    about: "wait for the first of several processes to exit",
    usage: "usage: caim first-exit --pid PID [--pid PID ...] [--guard SECONDS]\n",
    exec: super::wait,
};
