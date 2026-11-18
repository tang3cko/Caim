use super::Command;

pub const COMMAND: Command = Command {
    name: crate::script::MODE,
    aliases: &[],
    about: "run, check or describe Lua scripts",
    usage: "usage: caim script FILE\n       caim script --check FILE...\n       caim script --types\n",
    exec,
};

fn exec(args: &[String]) -> i32 {
    super::report(args, "", crate::script::dispatch(args))
}
