#!/usr/bin/env bash
set -euo pipefail
HOME_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Under TMPDIR, and resolved: macOS's /var and /tmp are symlinks, and caim names a project by its real path.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/caim-launcher.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
WORK="$(cd "$WORK" && pwd -P)"

fail() { echo "FAIL: $CASE: $*" >&2; exit 1; }
pass() { echo "PASS: launcher $CASE"; }
status() {
  local expected="$1" result=0
  shift
  "$@" > "$WORK/out" 2>&1 || result=$?
  [[ "$result" == "$expected" ]] || { cat "$WORK/out" >&2; fail "exit $result, expected $expected"; }
}
contains() { grep -qF -- "$1" "$WORK/out" || { cat "$WORK/out" >&2; fail "output lacks '$1'"; }; }

PROJECT="$WORK/project"
mkdir -p "$PROJECT/scripts" "$PROJECT/deep/er"
printf 'name = "launcher-test"\n\n[tasks.hello]\ndescription = "says hello"\nrun = ["scripts/hello"]\n' > "$PROJECT/caim.toml"
cat > "$PROJECT/scripts/hello" <<'TASK'
#!/usr/bin/env bash
echo "hello from $TASK_NAME in $PWD"
TASK
chmod +x "$PROJECT/scripts/hello"

CASE=build
status 0 "$HOME_DIR/caim" --build
contains "caim is current"
status 0 "$HOME_DIR/caim" --check
pass

CASE=no-project
status 78 env -u CAIM_ROOT -C "$WORK" "$HOME_DIR/caim" list
contains "no caim.toml in the current directory or above it"
pass

COPY="$WORK/Caim"
mkdir -p "$COPY/target/release"
cp -R "$HOME_DIR/src" "$HOME_DIR/Cargo.toml" "$HOME_DIR/Cargo.lock" "$HOME_DIR/mise.toml" "$HOME_DIR/caim" "$COPY/"
cp "$HOME_DIR/target/release/caim" "$HOME_DIR/target/release/caim.sources" "$COPY/target/release/"

CASE=stale-source
status 0 "$COPY/caim" --check
cp -p "$COPY/src/lib.rs" "$WORK/lib.rs"
printf '\n// changed\n' >> "$COPY/src/lib.rs"
touch -r "$WORK/lib.rs" "$COPY/src/lib.rs"   # the same mtime: only the content says it changed
status 1 "$COPY/caim" --check
contains "missing or stale"
cp -p "$WORK/lib.rs" "$COPY/src/lib.rs"
status 0 "$COPY/caim" --check
pass

CASE=new-source-file
: > "$COPY/src/extra.rs"   # a file the build would see changes the fingerprint even when it is empty
status 1 "$COPY/caim" --check
rm "$COPY/src/extra.rs"
status 0 "$COPY/caim" --check
pass

CASE=missing-binary
rm "$COPY/target/release/caim"
status 1 "$COPY/caim" --check
pass

CASE=list
status 0 env -u CAIM_ROOT -C "$PROJECT/deep/er" "$HOME_DIR/caim" list
contains "hello"
contains "says hello"
pass

CASE=run
status 0 env -u CAIM_ROOT -C "$PROJECT/deep/er" "$HOME_DIR/caim" hello
contains "hello from hello in $PROJECT"
contains "[hello] exit=0"
[[ "$(ls "$PROJECT")" == "$(printf 'caim.toml\ndeep\nscripts')" ]] || fail "caim wrote into the project: $(ls "$PROJECT")"
pass

CASE=bad-argument
status 64 env -u CAIM_ROOT -C "$PROJECT" "$HOME_DIR/caim" hello --bogus value
pass

echo "PASS: launcher, all cases"
