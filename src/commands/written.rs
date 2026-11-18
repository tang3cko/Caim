use super::Command;

pub const COMMAND: Command = Command {
    name: "written",
    aliases: &[],
    about: "wait for a file to be no longer empty",
    usage: "usage: caim written FILE [GUARD_SECONDS]\n",
    exec: super::wait,
};
