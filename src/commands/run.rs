use super::Command;

pub const COMMAND: Command = Command {
    name: "run",
    aliases: &[],
    about: "run a command under a stall guard",
    usage: "usage: caim run --idle SECONDS --output FILE --outcome FILE [--step SIGNAL:SECONDS ...] -- COMMAND...\n",
    exec: super::wait,
};
