use crate::error::{OK, USAGE};
use crate::tasks::{self, Task};
use std::collections::BTreeMap;
use std::io::Write;

pub const MODE: &str = "completions";

const BASH: &str = r#"_caim() {
  local IFS=$'\n'
  COMPREPLY=($(compgen -W "$("${COMP_WORDS[0]}" completions words "${COMP_WORDS[@]:1:COMP_CWORD-1}" 2>/dev/null)" -- "${COMP_WORDS[COMP_CWORD]}"))
}
complete -o default -F _caim caim
"#;

const ZSH: &str = r#"_caim() {
  local -a candidates
  candidates=("${(@f)$("${words[1]}" completions words "${(@)words[2,CURRENT-1]}" 2>/dev/null)}")
  compadd -a candidates
}
compdef _caim caim
"#;

pub fn dispatch(args: &[String]) -> i32 {
    let text = match args.get(1).map(String::as_str) {
        Some("bash") if args.len() == 2 => BASH.to_string(),
        Some("zsh") if args.len() == 2 => ZSH.to_string(),
        Some("words") => match tasks::load() {
            Ok(tasks) => candidates(&tasks, &args[2..]).iter().map(|word| format!("{word}\n")).collect(),
            Err(_) => String::new(),
        },
        _ => {
            eprintln!("usage: caim completions bash|zsh; load it with eval \"$(caim completions zsh)\"");
            return USAGE;
        }
    };
    // A shell that stops reading early is no error of ours.
    let _ = std::io::stdout().write_all(text.as_bytes());
    OK
}

pub fn candidates(tasks: &BTreeMap<String, Task>, words: &[String]) -> Vec<String> {
    match words.first().map(String::as_str) {
        Some("list") => return if words.len() == 1 { vec!["--json".into()] } else { Vec::new() },
        Some(MODE) => return if words.len() == 1 { vec!["bash".into(), "zsh".into()] } else { Vec::new() },
        _ => {}
    }
    let mut current: Option<&Task> = None;
    let mut value_of = None;
    for word in words {
        if value_of.take().is_some() {
            continue;
        }
        if let Some(name) = word.strip_prefix("--") {
            value_of = current
                .and_then(|task| task.declaration.args.iter().find(|arg| arg.name == name))
                .filter(|arg| !arg.flag);
        } else if let Some(task) = tasks.get(word) {
            current = Some(task);
        }
    }
    if let Some(arg) = value_of {
        return arg.choices.clone();
    }
    let mut found = Vec::new();
    match current {
        Some(task) => {
            found.extend(task.declaration.args.iter().map(|arg| format!("--{}", arg.name)));
            found.push("--help".into());
        }
        None if words.is_empty() => found.extend(["list", "--json", "--help"].map(String::from)),
        None => found.push("--help".into()),
    }
    found.extend(tasks.values().filter(|task| !task.declaration.hidden).map(|task| task.name.clone()));
    found
}
