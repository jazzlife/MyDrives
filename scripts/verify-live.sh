#!/usr/bin/env bash
# Exercise an already-mounted MyDrives drive the way a user does.
#
# verify-posix.sh mounts its own dev drive; this one takes a mount that the
# installed agent put there, so it checks the shipped binary against a real
# account:
#
#   scripts/verify-live.sh ~/MyDrives/GoogleDrive
#
# Sizes are deliberately small (1 MiB): a cloud backend pays a round trip per
# chunk, and the point here is correctness, not throughput.
set -uo pipefail

MNT="${1:?usage: verify-live.sh <mountpoint>}"
PASS=0
FAIL=0
FAILED=()

say() { printf '\n--- %s\n' "$*"; }
ok()  { PASS=$((PASS+1)); printf '  ok   %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); FAILED+=("$*"); printf '  FAIL %s\n' "$*"; }
# The checks run in a subshell: a loop that gives up with `exit 1` must fail its
# own check, not end the script and silently skip every later section.
check()  { if ( eval "$2" ) >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
refute() { if ( eval "$2" ) >/dev/null 2>&1; then bad "$1 (should have failed)"; else ok "$1"; fi; }

sum_file() { md5 -q "$1" 2>/dev/null || md5sum < "$1" | cut -d' ' -f1; }
set_size() { perl -e 'truncate($ARGV[0], $ARGV[1]) or die' "$1" "$2"; }

printf '==== %s ====\n' "$MNT"
check "the mount point exists"             "test -d '$MNT'"
check "it is in the mount table"           "mount | grep -F '$MNT'"
check "statfs answers (df)"                "df -k '$MNT' | tail -1 | grep -q ."
check "the root lists without an error"    "ls -A '$MNT'"
printf '  root entries: %s\n' "$(ls -A "$MNT" 2>/dev/null | wc -l | tr -d ' ')"

W="$MNT/mydrives-verify-$$"
say "directories"
check "mkdir"                              "mkdir '$W'"
[ -d "$W" ] || { echo "cannot create a working directory - stopping"; exit 1; }
check "mkdir -p (nested)"                  "mkdir -p '$W/a/b'"
check "the nested path stats"              "test -d '$W/a/b'"
check "the child is listed"                "ls '$W' | grep -q '^a$'"
refute "mkdir of an existing name"         "mkdir '$W/a'"
refute "rmdir of a non-empty directory"    "rmdir '$W/a'"

say "small files"
check "write with a redirect"              "echo 'hello mydrives' > '$W/hello.txt'"
check "read it back"                       "test \"\$(cat '$W/hello.txt')\" = 'hello mydrives'"
check "the size is right"                  "test \"\$(wc -c < '$W/hello.txt' | tr -d ' ')\" -eq 15"
check "append"                             "echo 'second line' >> '$W/hello.txt'"
check "the append is visible"              "test \"\$(wc -l < '$W/hello.txt' | tr -d ' ')\" -eq 2"
check "cp inside the mount"                "cp '$W/hello.txt' '$W/copy.txt'"
check "the copies are identical"           "cmp '$W/hello.txt' '$W/copy.txt'"
check "rename"                             "mv '$W/copy.txt' '$W/renamed.txt'"
check "the old name is gone"               "test ! -e '$W/copy.txt'"
check "rename into a subdirectory"         "mv '$W/renamed.txt' '$W/a/renamed.txt'"
check "unlink"                             "rm '$W/a/renamed.txt'"
refute "reading a missing file"            "cat '$W/does-not-exist'"

# Finder hands a filesystem decomposed Korean; these services store composed.
# A name that lists but will not open is the symptom when the two disagree.
say "non-ASCII names"
KO="$W/한글-이름.txt"
check "write a Korean filename"            "echo ko > '$KO'"
check "read it back by the same name"      "test \"\$(cat '$KO')\" = 'ko'"
check "it is listed"                       "ls '$W' | grep -q '한글-이름.txt'"
NFD=$(printf '%s' "$W/$(printf '\xe1\x84\x92\xe1\x85\xa1\xe1\x86\xab\xe1\x84\x80\xe1\x85\xb3\xe1\x86\xaf')-이름.txt")
check "open it by the decomposed spelling" "cat '$NFD'"
check "remove it"                          "rm '$KO'"

say "a 1 MiB file"
check "dd write"                           "dd if=/dev/urandom of='$W/big.bin' bs=262144 count=4 2>/dev/null"
check "the size is 1 MiB"                  "test \"\$(wc -c < '$W/big.bin' | tr -d ' ')\" -eq 1048576"
SUM_IN=$(sum_file "$W/big.bin")
check "re-reading gives the same bytes"    "test -n '$SUM_IN' && test \"\$(sum_file '$W/big.bin')\" = '$SUM_IN'"
check "random-access read"                 "test \"\$(dd if='$W/big.bin' bs=4096 skip=100 count=1 2>/dev/null | wc -c | tr -d ' ')\" -eq 4096"
check "copy out of the mount"              "cp '$W/big.bin' /tmp/mydrives-live-out.bin"
check "the copy out matches"               "test -n '$SUM_IN' && test \"\$(sum_file /tmp/mydrives-live-out.bin)\" = '$SUM_IN'"
check "truncate to 1 KiB"                  "set_size '$W/big.bin' 1024"
check "the truncated size"                 "test \"\$(wc -c < '$W/big.bin' | tr -d ' ')\" -eq 1024"
rm -f /tmp/mydrives-live-out.bin

FILES="${FILES:-20}"
say "$FILES files (enumeration)"
check "create them"                        "i=1; while [ \$i -le $FILES ]; do echo \$i > '$W/f'\$i'.txt' || exit 1; i=\$((i+1)); done"
check "all of them are listed"             "test \"\$(ls '$W' | grep -c '^f[0-9]*\.txt$' | tr -d ' ')\" -eq $FILES"
check "find walks the tree"                "test \"\$(find '$W' -type f | wc -l | tr -d ' ')\" -ge $FILES"
check "glob + rm"                          "rm '$W'/f*.txt"
check "they are gone"                      "test \"\$(ls '$W' | grep -c '^f[0-9]*\.txt$' | tr -d ' ')\" -eq 0"

say "concurrency (4 writers, then 4 readers)"
check "4 parallel writers" "pids=''; i=1; while [ \$i -le 4 ]; do (dd if=/dev/zero of='$W/p'\$i'.bin' bs=65536 count=4 2>/dev/null) & pids=\"\$pids \$!\"; i=\$((i+1)); done; rc=0; for p in \$pids; do wait \$p || rc=1; done; test \$rc -eq 0"
check "each one is 256 KiB" "i=1; while [ \$i -le 4 ]; do test \"\$(wc -c < '$W/p'\$i'.bin' | tr -d ' ')\" -eq 262144 || exit 1; i=\$((i+1)); done"
check "4 parallel readers" "pids=''; i=1; while [ \$i -le 4 ]; do (sum_file '$W/p'\$i'.bin' >/dev/null) & pids=\"\$pids \$!\"; i=\$((i+1)); done; rc=0; for p in \$pids; do wait \$p || rc=1; done; test \$rc -eq 0"

say "metadata and permissions"
check "stat works"                         "stat '$W/hello.txt'"
printf '  mode/owner: %s\n' "$(stat -f '%Sp %Su:%Sg' "$W/hello.txt" 2>/dev/null)"
check "the file is readable by the owner"  "test -r '$W/hello.txt'"
check "the file is writable by the owner"  "test -w '$W/hello.txt'"
check "the directory is writable"          "test -w '$W'"
check "touch sets an mtime"                "touch -t 202001020304 '$W/hello.txt'"
check "the mtime stuck"                    "test \"\$(stat -f %Sm -t %Y-%m-%d '$W/hello.txt')\" = '2020-01-02'"
check "chmod does not break the file"      "chmod 644 '$W/hello.txt' 2>/dev/null; test -r '$W/hello.txt'"

say "tools that stress read-modify-write"
if command -v tar >/dev/null; then
  check "tar create in the mount"          "tar -cf '$W/t.tar' -C '$W' hello.txt"
  check "tar extract in the mount"         "mkdir -p '$W/untar' && tar -xf '$W/t.tar' -C '$W/untar'"
  check "the extracted file matches"       "cmp '$W/hello.txt' '$W/untar/hello.txt'"
fi
if command -v sqlite3 >/dev/null; then
  check "sqlite3 create"                   "sqlite3 '$W/test.db' 'create table t(x);insert into t values(1),(2);'"
  check "sqlite3 read"                     "test \"\$(sqlite3 '$W/test.db' 'select count(*) from t;')\" = 2"
fi
if command -v git >/dev/null; then
  check "git init + commit in the mount" \
    "(git -C '$W' init -q repo && cd '$W/repo' && git config user.email e@e && git config user.name e && echo x > x && git add x && git commit -qm x)"
fi

say "cleanup"
check "recursive rm"                       "rm -rf '$W'"
check "the working directory is gone"      "test ! -e '$W'"

printf '\n==== %s: passed %d, failed %d\n' "$MNT" "$PASS" "$FAIL"
for f in "${FAILED[@]:-}"; do [ -n "$f" ] && printf '     failed: %s\n' "$f"; done
[ "$FAIL" -eq 0 ]
