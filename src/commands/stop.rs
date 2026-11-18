use super::Command;

pub const COMMAND: Command = Command {
    name: "stop",
    aliases: &[],
    about: "stop a process group by steps",
    usage: "usage: caim stop --group GROUP | --pid PID [--step SIGNAL:SECONDS ...]\n",
    exec: super::wait,
};
