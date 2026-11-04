#!/usr/bin/env bash
set -euo pipefail
HOME_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CAIM="${1:-$HOME_DIR/target/release/caim}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/caim-terminal.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
WORK="$(cd "$WORK" && pwd -P)"
: > "$WORK/go"
cat > "$WORK/caim.toml" <<TOML
name = "terminal-test"

[tasks.ask]
guard = false
run = "echo ready; read answer; echo \"got \$answer\""

[tasks.work]
run = "echo \$\$ > '$WORK/task.pid'; echo working; '$CAIM' written '$WORK/go' 600 > /dev/null; echo finished"
TOML
expect "$HOME_DIR/tests/terminal/job_control.exp" "$CAIM" "$WORK" "$WORK/go" "${2:-}"
