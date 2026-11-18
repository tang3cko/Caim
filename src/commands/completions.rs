use super::Command;

pub const COMMAND: Command = Command {
    name: crate::completions::MODE,
    aliases: &[],
    about: "print shell completion",
    usage: "usage: caim completions bash|zsh\n\nLoad it with eval \"$(caim completions zsh)\".\n",
    exec: crate::completions::dispatch,
};
