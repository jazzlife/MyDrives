#!/usr/bin/env bash
# End-to-end verification of a MyDrives mount on a POSIX host (macOS or Linux).
#
# Mounts through the real kernel - FUSE on Linux, or the in-box NFS client with
# --provider nfs (the default on macOS) - and then exercises it with ordinary
# shell commands. That is the point: user-mode filesystems break on the things
# users actually do, not on unit tests.
#
#   scripts/verify-posix.sh mem
#   PROVIDER=nfs scripts/verify-posix.sh mem
#   SMB_HOST=192.168.0.10 SMB_SHARE=data SMB_USER=u SMB_PASS=p \
#     scripts/verify-posix.sh smb
#
# Optional: SHARE_DIR=/srv/share also checks that the bytes really landed on the
# server (only possible when the server's directory is visible locally).
set -uo pipefail

KIND="${1:-mem}"
PROVIDER="${PROVIDER:-}"
# Deliberately not $TMPDIR: on macOS that is a long per-session path under
# /var/folders, and the mount table reports it with a /private prefix, which
# makes the checks below harder to read.
MNT="${MNT:-/tmp/mydrives-e2e-$KIND}"
DAEMON="${DAEMON:-target/debug/mydrivesd}"
LOG="${LOG:-/tmp/mydrives-e2e-$KIND.log}"
PASS=0
FAIL=0

say()  { printf '\n=== %s ===\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$*"; }
check()  { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
# Error semantics matter as much as success ones.
refute() { if eval "$2" >/dev/null 2>&1; then bad "$1 (should have failed)"; else ok "$1"; fi; }

# --- portability shims (macOS is BSD, Linux is GNU) -------------------------
sum_file() {
  if command -v md5sum >/dev/null 2>&1; then md5sum < "$1" | cut -d' ' -f1
  else md5 -q "$1"; fi
}
mtime_date() {   # YYYY-MM-DD of a file's mtime
  if stat -f %m "$1" >/dev/null 2>&1; then stat -f %Sm -t %Y-%m-%d "$1"
  else date -u -r "$1" +%Y-%m-%d 2>/dev/null || stat -c %y "$1" | cut -d' ' -f1; fi
}
set_size() { perl -e 'truncate($ARGV[0], $ARGV[1]) or die' "$1" "$2"; }
file_size() { wc -c < "$1" | tr -d ' '; }

cleanup() {
  if [ -n "${PID:-}" ] && kill -0 "$PID" 2>/dev/null; then
    kill -INT "$PID" 2>/dev/null
    for _ in $(seq 1 60); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
    kill -KILL "$PID" 2>/dev/null
  fi
}
trap cleanup EXIT

[ -x "$DAEMON" ] || { echo "build first: cargo build -p mydrives-daemon"; exit 2; }
mkdir -p "$MNT"

args=(dev-mount --kind "$KIND" --mountpoint "$MNT" --id "e2e-$KIND")
[ -n "$PROVIDER" ] && args+=(--provider "$PROVIDER")
if [ "$KIND" = "smb" ]; then
  : "${SMB_HOST:?SMB_HOST is required}" "${SMB_SHARE:?SMB_SHARE is required}"
  export MYDRIVES_E2E_PASS="${SMB_PASS:-}"
  args+=(--host "$SMB_HOST" --port "${SMB_PORT:-445}" --share "$SMB_SHARE"
         --user "${SMB_USER:-}" --password-env MYDRIVES_E2E_PASS)
fi

say "mounting ($KIND) at $MNT"
"$DAEMON" "${args[@]}" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 150); do
  grep -q "^mounted " "$LOG" 2>/dev/null && break
  kill -0 "$PID" 2>/dev/null || break
  sleep 0.2
done
if ! grep -q "^mounted " "$LOG"; then
  echo "mount failed; log:"; cat "$LOG"; exit 1
fi
grep -E "^(mounted|source)" "$LOG"
check "mount point is a directory"        "test -d '$MNT'"
# macOS reports /tmp mounts with a /private prefix, so match on the last element.
check "mount shows up in the mount table" "mount | grep -F '$(basename "$MNT")'"

W="$MNT/mydrives-e2e"
rm -rf "$W" 2>/dev/null
say "directories and listings"
check "mkdir"                      "mkdir '$W'"
check "mkdir -p (nested)"          "mkdir -p '$W/a/b/c'"
check "stat on a new directory"    "test -d '$W/a/b/c'"
check "listing shows children"     "ls '$W' | grep -q '^a$'"
refute "mkdir of an existing name" "mkdir '$W/a'"
refute "rmdir of a non-empty dir"  "rmdir '$W/a'"

say "small files"
check "write with a redirect"   "echo 'hello mydrives' > '$W/hello.txt'"
check "read it back"            "test \"\$(cat '$W/hello.txt')\" = 'hello mydrives'"
check "size is reported"        "test \"\$(wc -c < '$W/hello.txt' | tr -d ' ')\" -eq 15"
check "append"                  "echo 'second line' >> '$W/hello.txt'"
check "append is visible"       "test \"\$(wc -l < '$W/hello.txt' | tr -d ' ')\" -eq 2"
check "in-place edit (sed -i)"  "sed -i.bak 's/hello/HELLO/' '$W/hello.txt' && rm -f '$W/hello.txt.bak'"
check "edit took effect"        "grep -q HELLO '$W/hello.txt'"
check "cp into the mount"       "cp '$W/hello.txt' '$W/copy.txt'"
check "copies are identical"    "cmp '$W/hello.txt' '$W/copy.txt'"
check "rename"                  "mv '$W/copy.txt' '$W/renamed.txt'"
check "the old name is gone"    "test ! -e '$W/copy.txt'"
check "rename into a subdir"    "mv '$W/renamed.txt' '$W/a/renamed.txt'"
check "it is there"             "test -f '$W/a/renamed.txt'"
check "unlink"                  "rm '$W/a/renamed.txt'"
refute "reading a missing file" "cat '$W/does-not-exist'"

say "a big file (8 MiB - the copy path real users hit)"
check "dd write"              "dd if=/dev/urandom of='$W/big.bin' bs=1048576 count=8 2>/dev/null"
check "size is 8 MiB"         "test \"\$(wc -c < '$W/big.bin' | tr -d ' ')\" -eq 8388608"
SUM_IN=$(sum_file "$W/big.bin")
check "re-read is identical"  "test \"\$(sum_file '$W/big.bin')\" = '$SUM_IN'"
check "random access read"    "test \"\$(dd if='$W/big.bin' bs=4096 skip=1000 count=1 2>/dev/null | wc -c | tr -d ' ')\" -eq 4096"
check "copy out of the mount" "cp '$W/big.bin' '${TMPDIR:-/tmp}/mydrives-out.bin'"
check "the copy matches"      "test \"\$(sum_file '${TMPDIR:-/tmp}/mydrives-out.bin')\" = '$SUM_IN'"
check "truncate to 1 KiB"     "set_size '$W/big.bin' 1024"
check "truncated size"        "test \"\$(wc -c < '$W/big.bin' | tr -d ' ')\" -eq 1024"
rm -f "${TMPDIR:-/tmp}/mydrives-out.bin"

if [ -n "${SHARE_DIR:-}" ] && [ -d "$SHARE_DIR" ]; then
  say "server-side verification (the bytes really landed on the server)"
  check "the file exists on the server"    "test -f '$SHARE_DIR/mydrives-e2e/hello.txt'"
  check "its contents match on the server" "grep -q HELLO '$SHARE_DIR/mydrives-e2e/hello.txt'"
fi

# Over a WAN-latency link every create is several round trips, so allow a
# smaller count: FILES=50 scripts/verify-posix.sh smb
FILES="${FILES:-300}"
say "many files ($FILES - directory enumeration under load)"
# `|| exit 1` matters: without it a partially failed loop still reports success,
# which is how a silent write failure hides.
check "create $FILES files"   "i=1; while [ \$i -le $FILES ]; do echo \$i > '$W/f'\$i'.txt' || exit 1; i=\$((i+1)); done"
check "all $FILES are listed" "test \"\$(ls '$W' | grep -c '^f[0-9]*\.txt$' | tr -d ' ')\" -eq $FILES"
check "find walks the tree"   "test \"\$(find '$W' -type f | wc -l | tr -d ' ')\" -ge $FILES"
check "glob + rm"             "rm '$W'/f*.txt"
check "they are gone"         "test \"\$(ls '$W' | grep -c '^f[0-9]*\.txt$' | tr -d ' ')\" -eq 0"

say "concurrency"
# Note: `wait` with no arguments would also wait for the agent we started in the
# background, so each batch waits on its own PIDs.
check "8 parallel writers" "pids=''; i=1; while [ \$i -le 8 ]; do (dd if=/dev/zero of='$W/p'\$i'.bin' bs=65536 count=8 2>/dev/null) & pids=\"\$pids \$!\"; i=\$((i+1)); done; rc=0; for p in \$pids; do wait \$p || rc=1; done; test \$rc -eq 0"
check "every parallel file is 512 KiB" "i=1; while [ \$i -le 8 ]; do test \"\$(wc -c < '$W/p'\$i'.bin' | tr -d ' ')\" -eq 524288 || exit 1; i=\$((i+1)); done"
check "8 parallel readers" "pids=''; i=1; while [ \$i -le 8 ]; do (sum_file '$W/p'\$i'.bin' >/dev/null) & pids=\"\$pids \$!\"; i=\$((i+1)); done; rc=0; for p in \$pids; do wait \$p || rc=1; done; test \$rc -eq 0"

say "metadata"
check "statfs via df"     "df -k '$MNT' | tail -1 | grep -q ."
check "touch sets mtime"  "touch -t 202001020304 '$W/hello.txt'"
check "the mtime stuck"   "test \"\$(mtime_date '$W/hello.txt')\" = '2020-01-02'"
check "stat works"        "stat '$W/hello.txt'"

say "tools that stress read-modify-write"
if command -v tar >/dev/null; then
  check "tar create in the mount"  "tar -cf '$W/t.tar' -C '$W' hello.txt"
  check "tar extract in the mount" "mkdir -p '$W/untar' && tar -xf '$W/t.tar' -C '$W/untar'"
  check "the extracted file matches" "cmp '$W/hello.txt' '$W/untar/hello.txt'"
fi
if command -v sqlite3 >/dev/null; then
  check "sqlite3 create" "sqlite3 '$W/test.db' 'create table t(x);insert into t values(1),(2);'"
  check "sqlite3 read"   "test \"\$(sqlite3 '$W/test.db' 'select count(*) from t;')\" = 2"
fi
if command -v git >/dev/null; then
  # In a subshell: a `cd` into the mount would keep the script's working
  # directory inside a tree we are about to delete, and a deleted directory that
  # is still someone's cwd stays visible (POSIX, and NFS in particular).
  check "git init + commit in the mount" \
    "(git -C '$W' init -q repo && cd '$W/repo' && git config user.email e@e && git config user.name e && echo x > x && git add x && git commit -qm x)"
fi

say "cleanup and unmount"
check "recursive rm"              "rm -rf '$W'"
# Not "the mount is empty": the backend may legitimately have pre-existing
# content (the mem fixture does). Only our own working directory must be gone.
check "the work directory is gone" "test ! -e '$W'"

kill -INT "$PID"
for _ in $(seq 1 150); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
check "the process exited"      "! kill -0 '$PID' 2>/dev/null"
check "unmounted cleanly"       "! mount | grep -F '$(basename "$MNT")'"
check "the mount point is bare" "test -z \"\$(ls -A '$MNT' 2>/dev/null)\""
check "no panic in the log"     "! grep -qi 'panic' '$LOG'"
check "no unexpected errors"    "! grep -E 'ERROR (write|read|getattr|lookup) error' '$LOG'"
PID=""

printf '\n==================================\n'
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"
printf 'log: %s\n' "$LOG"
[ "$FAIL" -eq 0 ]
