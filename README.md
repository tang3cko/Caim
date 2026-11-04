# Caim

A task runner that stops a hung task by its lack of progress, not by a clock.

Long builds, imports and test runs sometimes hang. A timeout either kills a run that was merely slow or waits far too long for one that is stuck. Caim watches what a task declares as progress, new kinds of lines in its output or logs and new entries in its directories, and stops the run the moment that progress stops. It reports the stop as exit 124, as timeout(1) does, so a hang is never mistaken for a broken build. Otherwise it gets out of the way: the task's output is its output, and its exit status is its exit status.

## Features

- **Tasks in one file.** Declare tasks, their arguments, the tasks they require, their environment and directory in `caim.toml`; `caim list` shows them and `caim <task> --help` explains one.
- **A stall guard, not a timeout.** A run is stopped after `idle` seconds without progress, however long it has been running. A heartbeat that only moves a counter does not count as progress.
- **The command's own exit status.** Caim exits with the task's status and uses the conventional codes of timeout(1), the shell and sysexits(3) for what it does itself.
- **Concurrency groups.** Keep tasks that share an editor, a build directory or a device from running together, across every checkout and worktree of a project, exclusively or shared with a limit.
- **A well-behaved supervisor.** Each run is a process group of its own; TERM, INT, HUP and QUIT stop it in steps; Ctrl-Z suspends the run and `fg` resumes it.
- **Hooks.** A `lock` hook can hold a run back, and `after` hooks can change its status.
- **JSON events** for tools and agents with `--json`, and **bash and zsh completion**.

## Install

Download a release archive for your platform from [Releases](../../releases) (`caim-vX.Y.Z-<target>.tar.gz`, with `SHA256SUMS` and build provenance attestations), or install it with [mise](https://mise.jdx.dev):

```bash
mise use -g github:tang3cko/Caim
```

To build from source, with the Rust pinned in `mise.toml`:

```bash
cargo build --release --locked
```

## Quick start

Put a `caim.toml` at the root of your project:

```toml
name = "my-game"

[tasks.build]
description = "Build the game"
run = "dotnet build Game.csproj"

[tasks.play]
description = "Build and play"
requires = ["build"]
run = ["godot", "--path", "."]
guard = false

[tasks.package]
description = "The player package"
run = ["scripts/package"]
concurrency = "editor"
[[tasks.package.args]]
name = "platform"
help = "the target platform"
choices = ["Mac", "Win64"]
default = "Mac"
```

Then, from anywhere inside the project:

```console
$ caim list
  build                  Build the game
  package                The player package
  play                   Build and play
$ caim package --platform Win64
...
[package] exit=0
```

## Usage

```
caim list [--json]
caim <task> --help
caim [--json] <task> [--name value ...] [<task> [--name value ...] ...]
caim completions bash|zsh
```

Caim finds the project by the nearest `caim.toml` at or above the current directory (`CAIM_ROOT` overrides it) and runs tasks at the project's root. Several tasks on one line run in order, as make runs several targets; the first that does not succeed ends the line. A task runs at most once per line, whether it was named or required. Every task's arguments are checked before anything runs.

A run's stdout is the task's stdout and stderr, then one line, `[<task>] exit=N`, with the reason in parentheses when the command did not simply exit. Caim's own messages go to stderr. With `--json`, stdout is one JSON object per line: `start`, `output`, `hook`, `message`, `waiting` and, last for each task, `exit` (`exit`, `reason`). Every event names its `task`.

Caim sets up no tools: tasks run in the environment caim was started in. With mise, activate it in the shell or run `mise exec -- caim <task>`.

## Configuration

A task needs only `run`: a string goes through `sh -c`, a list is the command and its arguments with no shell. A command with a `/` in it is a path from the project's root; it may be built by a task this one requires.

| Key | Meaning | Default |
|---|---|---|
| `run` | The command | required |
| `description` | One line for `list` and `--help` | none |
| `group` | The heading `list` shows the task under | none |
| `requires` | Tasks run first, each to success, with their defaults | none |
| `args` | Arguments: `name`, `help`, and `choices`, `default`, `required` or `flag`; the command sees them as `TASK_ARG_<NAME>` | none |
| `guard` | Whether the stall guard watches the run; `false` gives the command the terminal | `true` |
| `idle` | Seconds without progress before the run is stopped | `GUARD_STALL_SECONDS`, else 180 |
| `stop` | Stop steps, `"SIGNAL:SECONDS"` (INT, TERM, HUP, QUIT or KILL) | `["TERM:20", "KILL:20"]` |
| `watch.logs`, `watch.dirs` | Progress sources besides the output, relative to the project | the output only |
| `env` | Variables for this task's command alone | none |
| `dir` | The directory inside the project the command runs in | the project's root |
| `lock` | A hook that must succeed before the run starts | none |
| `after` | Hooks run after the command, each able to change its status | none |
| `concurrency` | The groups the run holds | none |
| `human` | Marks a task a person runs rather than an agent | `false` |
| `hidden` | Leaves the task out of `list` | `false` |

A hook is declared under `[hooks.<name>]` with its own `run`. A lock hook that fails ends the run with its status; an after hook sees `TASK_STATUS` and exits 0 to leave it, or with the status the run should end with. A command can also declare progress sources while it runs, by appending `log /absolute/path` or `dir /absolute/path` to the file `GUARD_DECLARED` names.

### Concurrency

```toml
concurrency = "editor"            # the group to itself, waiting while another run holds it

[concurrency]                     # the same, spelled out
group = "binaries"
mode = "shared"                   # "exclusive" (the default) or "shared"
limit = 4                         # shared holders at once
scope = "project"                 # "project" (the default), "checkout" or "machine"
on_busy = "refuse"                # "wait" (the default) or "refuse": exit 75 at once
```

`project` is every checkout whose `caim.toml` gives the same name, worktrees included; `checkout` is one directory; `machine` is every project of the user. A waiting run says who holds the group and waits for its release, never for a time. A run passes its groups to the tasks it requires and to a caim its command starts, so neither waits on it. The locks are flock(2)ed files under `~/Library/Caches/caim/locks` on macOS and `$XDG_CACHE_HOME/caim/locks` elsewhere (`CAIM_LOCK_DIR` overrides it); the kernel frees them when a holder dies.

### Environment

| Variable | Seen by | Holds |
|---|---|---|
| `TASK_NAME` | the command, hooks | the task's name |
| `TASK_ARG_<NAME>` | the command, hooks | each argument, upper-cased with `-` as `_` |
| `TASK_STATUS`, `TASK_STARTED` | after hooks | the status so far, and a file whose time marks the start |
| `GUARD_DECLARED` | the command | the file to append progress sources to |
| `GUARD_STALL_SECONDS` | the command | the idle time in effect |
| `GUARD_WATCHERS`, `GUARD_LAUNCH_REGISTRY`, `CAIM_HELD` | the command | for a caim, or a launched app, the command starts |

Caim reads `CAIM_ROOT`, `CAIM_LOCK_DIR`, `GUARD_STALL_SECONDS`, `XDG_CACHE_HOME` and `TMPDIR`.

## Exit status

A task that ran exits with its command's status, 128 plus the signal when a signal ended it. Caim's own statuses stay clear of the 1 and 2 most commands use:

| Status | Meaning |
|---|---|
| 64 | the arguments did not match the declaration |
| 75 | a concurrency group with `on_busy = "refuse"` was held |
| 78 | `caim.toml`, a task's `dir` or `GUARD_STALL_SECONDS` is invalid |
| 124 | the run stalled and was stopped |
| 125 | caim itself failed |
| 126 | the command could not be executed |
| 127 | the command was not found |
| 128+N | caim was interrupted by signal N and stopped the run |

A required task or a lock hook that does not succeed ends the run with its status.

## Shell completion

```bash
eval "$(caim completions zsh)"    # or bash; for zsh, after compinit
```

Completes `list`, task names, a task's arguments and their choices, from the project the shell is in.

## Helper modes

The same binary carries the waiting modes shell scripts use (`exit`, `first-exit`, `written`, `line`, `activity`, `run`, `stop`) and SHA-256 modes (`hash`, `hash-check`). Their statuses are their own: a hang guard that runs out is 3.

## Platforms

macOS, and Linux 5.3 or later. Built from source, Linux needs glibc 2.29 or later; the released Linux binaries need glibc 2.43. Everything that depends on the operating system is in `src/native/`, one file per platform.

## Development

This repository is a caim project: `./caim` builds caim from these sources when they change, then runs it.

```bash
./caim check                  # lint, test, launcher and terminal, as CI does
./caim fmt                    # cargo fmt
./caim test --filter NAME     # one test
./caim linux --task test      # a task on Linux, in an Apple container
```

`./caim --build` only brings the binary up to date, and `./caim --check` exits 1 when it is stale. Builds run offline with every dependency pinned, so the crates must be in the local cargo cache. A release is made by raising `version` in `Cargo.toml` on main.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
