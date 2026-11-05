#!/usr/bin/env bash
# tasks/crate-check against real crates.io crates and their upstream repositories, and against tampered
# copies of them. It downloads crates, so run it somewhere disposable: caim linux --task crate-check-test.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHECK="$ROOT/tasks/crate-check"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/crate-check.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
DOWNLOADS="$WORK/downloads"
mkdir -p "$DOWNLOADS"

fail() { echo "FAIL: crate-check $CASE: $*" >&2; exit 1; }
pass() { echo "PASS: crate-check $CASE"; }
status() {
  local expected="$1" result=0
  shift
  "$@" > "$WORK/out" 2>&1 || result=$?
  [[ "$result" == "$expected" ]] || { cat "$WORK/out" >&2; fail "exit $result, expected $expected"; }
}
contains() { grep -qF -- "$1" "$WORK/out" || { cat "$WORK/out" >&2; fail "output lacks '$1'"; }; }
lacks() { ! grep -qF -- "$1" "$WORK/out" || { cat "$WORK/out" >&2; fail "output has '$1'"; }; }
check() { env CRATE_CHECK_DOWNLOADS="$DOWNLOADS" "$CHECK" "$@"; }

# Cargo.lock as released in 0.2.0 and 0.3.0: libc and the toml crates moved between them.
git -C "$ROOT" -c safe.directory='*' show v0.2.0:Cargo.lock > "$WORK/base.lock"
git -C "$ROOT" -c safe.directory='*' show v0.3.0:Cargo.lock > "$WORK/head.lock"

# set_package LOCK NAME VERSION SUM: LOCK with package NAME at VERSION and checksum SUM.
set_package() {
  awk -v name="$2" -v version="$3" -v sum="$4" '
    /^name = / { mine = ($3 == "\"" name "\"") }
    mine && /^version = / { $0 = "version = \"" version "\"" }
    mine && /^checksum = / { $0 = "checksum = \"" sum "\"" }
    { print }
  ' "$1"
}

# tamper NAME VERSION EDIT: the real crate with EDIT run inside it, left in DOWNLOADS; prints its sha256.
tamper() {
  local name="$1" version="$2" edit="$3" dir="$WORK/tamper"
  rm -rf "$dir" && mkdir -p "$dir"
  curl -fsSL -o "$dir/real.crate" "https://static.crates.io/crates/$name/$name-$version.crate"
  tar -xzf "$dir/real.crate" -C "$dir"
  (cd "$dir/$name-$version" && eval "$edit")
  tar -czf "$DOWNLOADS/$name-$version.crate" -C "$dir" "$name-$version"
  sha256sum "$DOWNLOADS/$name-$version.crate" | cut -d' ' -f1
}

CASE=unchanged
status 0 check "$WORK/base.lock" "$WORK/base.lock"
contains "no crates.io package changed"
pass

CASE=released-update
status 0 check "$WORK/base.lock" "$WORK/head.lock"
for crate in "libc 0.2.191" "toml 1.1.8+spec-1.1.0" "serde_spanned 1.1.2" "toml_datetime 1.1.2+spec-1.1.0" \
  "toml_parser 1.1.5+spec-1.1.0" "toml_writer 1.2.0+spec-1.1.0"; do
  contains "OK $crate"
done
lacks "FAIL"
lacks "REVIEW"
pass

CASE=checksum
set_package "$WORK/head.lock" libc 0.2.191 "$(printf '0%.0s' {1..64})" > "$WORK/case.lock"
status 1 check "$WORK/base.lock" "$WORK/case.lock"
contains "FAIL libc 0.2.191: sha256 differs from Cargo.lock"
pass

CASE=changed-source
sum="$(tamper libc 0.2.191 'echo "// changed" >> src/lib.rs')"
set_package "$WORK/head.lock" libc 0.2.191 "$sum" > "$WORK/case.lock"
status 1 check "$WORK/base.lock" "$WORK/case.lock"
contains "FAIL libc 0.2.191: src/lib.rs differs from https://github.com/rust-lang/libc"
contains "OK toml 1.1.8+spec-1.1.0"
pass

CASE=file-not-upstream
sum="$(tamper libc 0.2.191 'echo "fn payload() {}" > src/extra.rs')"
set_package "$WORK/head.lock" libc 0.2.191 "$sum" > "$WORK/case.lock"
status 1 check "$WORK/base.lock" "$WORK/case.lock"
contains "FAIL libc 0.2.191: src/extra.rs is not in https://github.com/rust-lang/libc"
pass

# libc 0.2.191 really has a build script; an old libc without one makes it a gained build script.
CASE=gained-build-script
rm -f "$DOWNLOADS"/*
sum="$(tamper libc 0.2.190 'rm build.rs; sed -i "/^build = /d" Cargo.toml')"
set_package "$WORK/base.lock" libc 0.2.190 "$sum" > "$WORK/case-base.lock"
status 1 check "$WORK/case-base.lock" "$WORK/head.lock"
contains "REVIEW libc 0.2.191: gained a build script since 0.2.190"
pass

# serde_derive 1.0.229 really is a proc macro; an old one without the flag makes it a new proc macro.
CASE=became-proc-macro
rm -f "$DOWNLOADS"/*
sum="$(tamper serde_derive 1.0.228 'sed -i "/^proc-macro = true/d" Cargo.toml')"
set_package "$WORK/base.lock" serde_derive 1.0.228 "$sum" > "$WORK/case-base.lock"
status 1 check "$WORK/case-base.lock" "$WORK/head.lock"
contains "REVIEW serde_derive 1.0.229: became a proc macro since 1.0.228"
pass

CASE=new-crate
rm -f "$DOWNLOADS"/*
curl -fsSL -o "$WORK/itoa.crate" https://static.crates.io/crates/itoa/itoa-1.0.15.crate
{
  cat "$WORK/head.lock"
  printf '\n[[package]]\nname = "itoa"\nversion = "1.0.15"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "%s"\n' \
    "$(sha256sum "$WORK/itoa.crate" | cut -d' ' -f1)"
} > "$WORK/case.lock"
status 1 check "$WORK/base.lock" "$WORK/case.lock"
contains "REVIEW itoa 1.0.15: new to Cargo.lock"
pass

CASE=not-crates-io
sed 's#^source = "registry+https://github.com/rust-lang/crates.io-index"$#source = "git+https://example.com/x"#' \
  "$WORK/head.lock" > "$WORK/case.lock"
status 1 check "$WORK/base.lock" "$WORK/case.lock"
contains "REVIEW libc 0.2.191: not from crates.io"
pass
