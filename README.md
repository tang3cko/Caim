# Caim

A task runner and process supervisor for a project's tasks. Caim judges whether a running task is making progress only by its output and the signs it declared (lines appended to a log, directories it watches), and the moment those stop, it stops the run and reports it VOID. Elapsed time never decides a verdict. Caim keeps nothing itself: a task's output and verdict go to stdout, and where a task puts what it makes is up to the task.

## Platforms

macOS (kqueue and libproc). Everything that depends on the operating system is in `src/native/`, one file per platform.

## Usage

Put `caim` on your PATH. A project is the directory holding a `caim.toml`, which names it. `caim` looks for the `caim.toml` in the current directory and then in each one above it, and runs every task at the project's root unless the task names a `dir`; `CAIM_ROOT` names the project instead.

Every task is declared in `caim.toml`; `run` is the only key a task needs. A `run` string goes through `sh -c`; a list is the command and its arguments, with no shell. A command with a `/` in it, such as a script of the project's, is a path from the project's root. A task's must be executable when the task runs, after what it requires, so a task can run what another one builds; one that is not ends the run as SETUP. A hook's must be executable when caim starts.

```toml
name = "hello-world"

[tasks.build]
run = "dotnet build Game.csproj"

[tasks.play]
description = "build and play"
group = "dev"
requires = ["build"]
run = ["godot", "--path", "."]
guard = false

[tasks.package]
description = "the player package"
group = "release"
run = ["scripts/package"]
lock = "editor-closed"
[[tasks.package.args]]
name = "platform"
help = "the target platform"
choices = ["Mac", "Win64"]
default = "Mac"

[hooks.editor-closed]
run = ["scripts/editor-closed"]
```

- `caim list [--json]`, `caim <task> --help`, `caim [--json] <task> [--name value ...] [<task> [--name value ...] ...]`. Options belong to the task before them. Every task's arguments are checked against its declaration before anything runs; a mismatch exits 64 with nothing started. The command sees them as `TASK_ARG_<NAME>`.
- Several tasks run in the order given, as make runs several targets: the first that does not pass ends the line, and caim exits with its status. A task runs at most once per line, whether it was named or required.
- `group` only says where `list` shows a task; tasks without one come first.
- A guarded task (the default; `guard = false` gives the command the terminal, its foreground included when caim has it, and no stall guard) is stopped as VOID once it has made no progress for `idle` seconds: the task's own `idle`, else `GUARD_STALL_SECONDS` from the environment, else 180. A stalled or interrupted run is stopped by `stop`, steps of `"SIGNAL:SECONDS"` (INT, TERM, HUP, QUIT or KILL, and how long its processes have to leave before the next step); `["TERM:20", "KILL:20"]` when not given.
- `env`, a table, is set for the task's command alone: not for what it requires, nor for the tasks after it. It cannot set `TASK_`, `GUARD_` or `CAIM_` names, which are caim's. `dir` is the directory inside the project the command runs in, the project's root when not given; hooks always run at the root.
- A run's stdout is the task's stdout and stderr, then one line, `[<task>] VERDICT PASS|FAIL|VOID|BLOCKED|SETUP|USAGE exit=N`, with the reason in parentheses when the command did not simply exit. Caim's own account of a stall or a stop goes to stderr. caim exits with the status of the line's last task run (see [Verdicts and exit statuses](#verdicts-and-exit-statuses)).
- With `--json`, stdout is one JSON object per line: `start` (`pid`), `output` (`line`), `hook` (`hook`, `line`), `message` (`text`), `waiting` and, last for each task run, `verdict` (`verdict`, `exit`, `reason`: `exited`, `stalled`, `interrupted`, `unavailable`, `not-started`, `required`, `lock` or `busy`). Every event names its `task`.
- A task's `watch.logs` and `watch.dirs` are paths relative to the project. A task can also declare one while it runs by appending `log /absolute/path` or `dir /absolute/path` to the file `GUARD_DECLARED` names.
- A hook, declared under `[hooks.<name>]` with its own `run`, can hold a run back (a task's `lock`) or change its status (`after`). A lock hook that exits non-zero ends the run with its status before the command starts; an after hook sees `TASK_STATUS` and exits 0 to leave it, or with the status the run should end with.
- A task can declare the concurrency groups it holds while it runs (see below).
- Tasks run in the environment caim was started in; caim sets up no tools. With mise, activate it in the shell or run `mise exec -- caim <task>`.
- The same binary carries the waiting modes shell scripts use (exit, first-exit, written, line, activity, run, stop) and the SHA-256 modes (hash, hash-check).

### Shell completion

`caim completions bash` and `caim completions zsh` print a completion script; load it from the shell's startup file, after `compinit` for zsh:

```bash
eval "$(caim completions zsh)"    # or bash
```

It completes `list`, task names, a task's `--arguments` and their `choices`, from the `caim.toml` of the directory the shell is in, and so for `./caim` too.

### Declaring a task

| Key | What it says | When not given |
|---|---|---|
| `run` | The command: a string through `sh -c`, or a list with no shell | required |
| `description` | One line for `list` and `--help` | none |
| `group` | Where `list` shows it | listed first |
| `requires` | Tasks run first, each to a pass, with their defaults | none |
| `args` | Its arguments: `name`, `help`, and `choices`, `default`, `required`, or `flag` (no value; `1` when given) | none |
| `guard` | Whether the stall guard watches it; `false` gives it the terminal | `true` |
| `idle` | Seconds without progress before a stall | `GUARD_STALL_SECONDS`, else 180 |
| `stop` | Stop steps, `"SIGNAL:SECONDS"` | `["TERM:20", "KILL:20"]` |
| `watch.logs`, `watch.dirs` | Progress sources besides its output, relative to the project | its output only |
| `env` | Variables for its command alone | none |
| `dir` | The directory inside the project it runs in | the project's root |
| `lock` | A hook that must pass before it starts | none |
| `after` | Hooks run after it, in order, each able to change its status | none |
| `concurrency` | The groups it holds while it runs | none |
| `human` | A person runs it, not an agent; `list` and `--help` say so | `false` |
| `hidden` | Left out of `list`, for a step other tasks require | `false` |

### What a task and a hook see

| Variable | Seen by | What it holds |
|---|---|---|
| `TASK_NAME` | the command, every hook | the task's name |
| `TASK_ARG_<NAME>` | the command, every hook | each argument given or defaulted, the name upper-cased with `-` as `_` |
| `TASK_STATUS` | after hooks | the status so far |
| `TASK_STARTED` | after hooks | a file created as the run started, whose time marks it |
| `GUARD_DECLARED` | the command | the file to append `log PATH` or `dir PATH` lines to |
| `GUARD_STALL_SECONDS` | the command | the idle time in effect, which a caim the command starts uses too |
| `GUARD_WATCHERS` | the command | where a stall watcher the command starts registers, so this run defers to it while it judges |
| `GUARD_LAUNCH_REGISTRY` | the command | a file of `pid<TAB>start time<TAB>command pattern` lines naming processes started through LaunchServices, stopped with the run |
| `CAIM_HELD` | the command | the concurrency groups this run holds, so a caim it starts does not wait on them |

caim itself reads `CAIM_ROOT`, `CAIM_LOCK_DIR`, `GUARD_STALL_SECONDS`, `XDG_CACHE_HOME` and `TMPDIR`.

### Verdicts and exit statuses

| Verdict | Exit status | Meaning |
|---|---|---|
| PASS | 0 | the command exited 0 |
| FAIL | any other | the command exited with that status |
| SETUP | 2 | the project or the run could not be set up (caim.toml, a missing program or dir) |
| VOID | 3 | the run says nothing about the task: it stalled, was interrupted (TERM, INT, HUP or QUIT to caim), or its watch broke |
| BLOCKED | 4 | a concurrency group with `on_busy = "refuse"` was held |
| USAGE | 64 | the arguments did not match the declaration, or the task judged its own and exited 64 |

A command that exits 2, 3, 4 or 64 itself is shown under that verdict. The waiting modes have statuses of their own: `run` exits 0 whenever the command exited, whatever its status, and writes how it ended (`exit N`, `stall status=N`, `interrupted SIG status=N`, `unavailable status=N`) to its `--outcome` file; it exits 3 on a stall, 128 plus the signal when interrupted, and 1 when its watch broke.

### How caim differs

caim never ends a run on elapsed time: a run ends when its command exits, when it is interrupted, or when its declared progress stops for the idle time, and a stopped run is VOID rather than FAIL, so a hang is never read as a broken build. Its concurrency groups cover every checkout of a project, take shared holders with a limit, and can refuse at once (BLOCKED) instead of waiting.

### Concurrency

A task that declares no group runs alongside anything. One that does holds the group for its run, whichever directory it was started from:

```toml
concurrency = "editor"            # the group to itself, waiting while another run holds it

[concurrency]                     # the same, spelled out
group = "binaries"
mode = "shared"                   # "exclusive" (the default) or "shared": shared holders run together, never with an exclusive one
limit = 4                         # shared holders at once (shared only; no limit without it)
scope = "project"                 # "project" (the default), "checkout" or "machine"
on_busy = "refuse"                # "wait" (the default) or "refuse": end BLOCKED (exit 4) at once, naming the holders
```

`concurrency` may also be a list of groups; every run takes them in one order, so two runs never hold what the other waits for. The scope decides who shares a name: `project` is every checkout whose `caim.toml` gives the same name (a git worktree or another clone of the project included), `checkout` is one directory, `machine` is every project of this user. A waiting run says who holds the group (task, pid, checkout, since when) and waits for a release, never for a time; with `--json` that is a `waiting` event, and a refused run's `verdict` carries `concurrency` and `holders`. A run passes its groups on to the tasks it requires and to a caim its command starts (`CAIM_HELD`), so neither waits on it.

The locks are flock(2)ed files under `~/Library/Caches/caim/locks` (`$XDG_CACHE_HOME/caim/locks` elsewhere, or `CAIM_LOCK_DIR`), never in the project. The kernel frees them when a holder dies, and nothing a command leaves running inherits them. A group only sees runs that go through caim; a resource used outside it, such as an editor opened by hand, is what a `lock` hook checks. Waiting runs are not served in order, and a steady stream of shared holders can keep an exclusive one waiting.

## Building

For working on caim itself, the `caim` script in this repository runs caim from its sources: it rebuilds the release binary in `target/` only when the fingerprint of `src`, `Cargo.toml`, `Cargo.lock` and `mise.toml` differs from the last build's. Every dependency is pinned to an exact version and cargo runs offline, so the crates must already be in the local cargo cache. Rust is pinned in this repository's `mise.toml` (run `mise trust` once).

- `./caim --build`: only brings the binary up to date.
- `./caim --check`: only says whether it is up to date (exit 3 when stale).

Prebuilt binaries are on the [Releases](../../releases) page (`caim-vX.Y.Z-<target>.tar.gz`, with `SHA256SUMS` and build provenance attestations).

## Development

This repository is a caim project too: its `caim.toml` declares the checks CI runs (`.github/workflows/ci.yaml`), and `./caim` runs them from the sources being worked on.

```bash
./caim check       # all of the below
./caim lint        # cargo fmt --check, clippy -D warnings, shellcheck
./caim test        # cargo test --locked
./caim launcher    # ./caim's freshness checks, and list and run on a scratch project
./caim fmt         # cargo fmt, in place
```

The tasks use the cargo and shellcheck on PATH (CI pins shellcheck 0.11.0); without mise active in the shell, `mise exec -- ./caim <task>` gives them the pinned Rust.

## Releases

The `version` in `Cargo.toml` is the release. When a version reaches main without a `vX.Y.Z` tag, `.github/workflows/release.yaml` tests, builds every target, writes checksums and provenance attestations, and creates the GitHub Release, which creates the tag. A push that leaves the version alone releases nothing. Release jobs use no caches.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
