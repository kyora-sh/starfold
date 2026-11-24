#!/bin/sh
# guard-test.sh: the server packages' process guard (GUARD in
# python/starfold-server/src/starfold_server/_native.py, the same text as packages/server/src/native.ts)
# under this system's /bin/sh, with stand-in servers (no binary):
#   1. its stdin pipe closes: the server is stopped, the guard exits with the server's status;
#   2. the process holding that pipe is SIGKILLed: no server and no watcher are left;
#   3. the server exits by itself: the guard exits with its status at once and leaves no watcher;
#   4. the same for a server that exits at once (and 5., with STARFOLD_BIN set, the real binary given
#      a flag it refuses).
# Each time nothing may be left in the guard's process group; in 3 to 5 that is checked while its stdin
# is still open, so only the guard can have ended the watcher. Needs perl (setsid; macOS has no command).
# Run as `sh .github/release/guard-test.sh` from anywhere; GUARD_SH names another shell to run the guard
# with (default /bin/sh, the one the SDKs use).
set -u
command -v perl >/dev/null || { echo "guard-test: needs perl, to start the guard in its own session as the SDKs do"; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
guard=$(awk '/^GUARD = """/ { on = 1; sub(/^GUARD = """/, ""); print; next } on && /^"""/ { exit } on' \
	"$root/python/starfold-server/src/starfold_server/_native.py")
[ -n "$guard" ] || { echo "guard-test: no GUARD in _native.py"; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail=0
bad() {
	echo "FAIL: $*"
	fail=1
}
alive() { kill -0 "$1" 2>/dev/null; }
gone_within() {
	i=0
	while alive "$1"; do
		i=$((i + 1))
		[ $i -gt 100 ] && return 1
		sleep 0.1
	done
}
left_in() { ps -A -o pgid= -o pid= | awk -v g="$1" '$1 == g { print $2 }'; }
group_empty_within() {
	i=0
	while [ -n "$(left_in "$1")" ]; do
		i=$((i + 1))
		[ $i -gt 30 ] && return 1
		sleep 0.1
	done
}

# A server that writes its pid, leaves on TERM with status 7, or by itself after $2 seconds with status 5.
cat >"$work/server" <<'EOF'
#!/bin/sh
trap 'exit 7' TERM
echo $$ >"$1"
n=0
while [ "$n" -lt "$2" ]; do sleep 1; n=$((n + 1)); done
exit 5
EOF
chmod +x "$work/server"

start() { run "$work/server" "$work/pid" "$1"; }
run() {
	rm -f "$work/pid" "$work/in"
	mkfifo "$work/in"
	# In its own session, as the SDKs start it: the guard signals its process group. GUARD_SH may hold
	# a shell and its options (bash --posix), so it is split on purpose.
	# shellcheck disable=SC2086
	perl -e 'use POSIX; POSIX::setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"' \
		${GUARD_SH:-/bin/sh} -c "$guard" "$@" <"$work/in" >/dev/null 2>&1 &
	g=$!
}
# The guard ends by itself while its stdin stays open; then nothing may be left in its group.
exits_with() {
	n=$1 want=$2
	exec 8>"$work/in"
	gone_within "$g" || bad "$n: the guard didn't exit after its server did"
	wait "$g"
	rc=$?
	[ -z "$want" ] || [ "$rc" = "$want" ] || bad "$n: the guard exited with $rc, not the server's $want"
	group_empty_within "$g" || bad "$n: left in the guard's group with its stdin open: $(left_in "$g" | tr '\n' ' ')"
	exec 8>&-
}
server_pid() {
	i=0
	while [ ! -s "$work/pid" ]; do
		i=$((i + 1))
		[ $i -gt 100 ] && return 1
		sleep 0.1
	done
	cat "$work/pid"
}

# 1. the pipe closes
start 600
exec 8>"$work/in"
s=$(server_pid) || bad "1: the server never started"
exec 8>&-
wait "$g"
rc=$?
[ "$rc" = 7 ] || bad "1: the guard exited with $rc, not the server's 7"
alive "$s" && bad "1: server $s still running"
group_empty_within "$g" || bad "1: left in the guard's group: $(left_in "$g" | tr '\n' ' ')"
echo "1. pipe closed: guard exited $rc, server gone, nothing left in its group"

# 2. the process holding the pipe is SIGKILLed
start 600
sleep 600 >"$work/in" &
holder=$!
s=$(server_pid) || bad "2: the server never started"
kill -KILL "$holder"
wait "$holder" 2>/dev/null
gone_within "$s" || bad "2: server $s outlived the pipe's holder"
gone_within "$g" || bad "2: guard $g outlived the pipe's holder"
wait "$g" 2>/dev/null
group_empty_within "$g" || bad "2: left in the guard's group: $(left_in "$g" | tr '\n' ' ')"
echo "2. holder SIGKILLed: server and guard gone, nothing left in its group"

# 3. the server exits by itself
start 1
exits_with 3 5
echo "3. server exited by itself: guard exited $rc, nothing left in its group"

# 4. the server exits at once
start 0
exits_with 4 5
echo "4. server exited at once: guard exited $rc, nothing left in its group"

# 5. the real server refuses a flag
if [ -n "${STARFOLD_BIN:-}" ]; then
	run "$STARFOLD_BIN" --no-such-flag
	exits_with 5 ""
	[ "$rc" != 0 ] || bad "5: the guard exited 0 for a refused flag"
	echo "5. starfold refused a flag: guard exited $rc, nothing left in its group"
fi

[ $fail = 0 ] && echo "guard-test: all passed with ${GUARD_SH:-/bin/sh}"
exit $fail
