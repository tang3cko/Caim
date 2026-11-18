use super::Command;

pub const COMMAND: Command = Command {
    name: "exit",
    aliases: &[],
    about: "wait for a process to exit",
    usage: "usage: caim exit PID [GUARD_SECONDS]\n",
    exec: super::wait,
};
