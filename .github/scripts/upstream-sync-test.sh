#!/usr/bin/env bash
# Usage: upstream-sync-test.sh
#
# Runs upstream-sync.sh against scratch upstream and origin repositories and checks
# which outcomes end the run green (exit 0) and which red.
# shellcheck disable=SC2016,SC2034
set -euo pipefail

script=$(cd "$(dirname "$0")" && pwd)/upstream-sync.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=bot GIT_COMMITTER_EMAIL=bot@example.com SYNC_BOT_EMAIL=bot@example.com

failures=0
check() {
	if eval "$2"; then echo "ok   $1"; else
		echo "FAIL $1"
		failures=$((failures + 1))
	fi
}

commit_files() {
	local repo=$1 msg=$2
	shift 2
	while [ $# -gt 0 ]; do
		mkdir -p "$repo/$(dirname "$1")"
		printf '%s\n' "$2" >"$repo/$1"
		shift 2
	done
	git -C "$repo" add -A
	git -C "$repo" commit -q -m "$msg"
}

setup() {
	dir=$root/$1
	mkdir -p "$dir"
	git init -q -b main "$dir/upstream"
	commit_files "$dir/upstream" init src/main.rs 'fn main() { voxl::run() }' web/app.ts 'export {};' README.md voxl
	git init -q -b main "$dir/seed"
	commit_files "$dir/seed" init crates/starfold-server/src/main.rs 'fn main() { starfold::run() }' \
		packages/web/app.ts 'export {};' README.md voxl .github/upstream-sha "$(git -C "$dir/upstream" rev-parse HEAD)"
	git init -q --bare -b main "$dir/origin.git"
	git -C "$dir/seed" push -q "$dir/origin.git" main
	git clone -q "$dir/origin.git" "$dir/work"
}

run_sync() {
	local url=${1:-$dir/upstream} body=${2:-$dir/body.md}
	: >"$dir/output"
	: >"$dir/summary"
	rc=0
	(cd "$dir/work" && GITHUB_OUTPUT=$dir/output GITHUB_STEP_SUMMARY=$dir/summary "$script" "$url" "$body") \
		>"$dir/stdout" 2>"$dir/stderr" || rc=$?
}

has_branch() { git -C "$dir/origin.git" rev-parse --verify --quiet refs/heads/sync/upstream >/dev/null; }

setup blocked
commit_files "$dir/upstream" 'fix: 100% sure run' src/main.rs 'fn main() { voxl::run(); }'
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
stop=$(git -C "$dir/upstream" rev-parse --short HEAD~1)
run_sync
check 'blocked: exits 0' '[ $rc -eq 0 ]'
check 'blocked: status=blocked' 'grep -qx status=blocked "$dir/output"'
check 'blocked: one warning naming the commit' \
	'[ "$(grep -c "^::warning::" "$dir/stdout")" = 1 ] && grep -q "^::warning::.*$stop fix: 100%25 sure run" "$dir/stdout"'
check 'blocked: summary names the commit' 'grep -qF "**Stopped at $stop fix: 100% sure run**" "$dir/summary"'
check 'blocked: nothing pushed' '! has_branch'

setup pushed
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
run_sync
check 'pushed: exits 0' '[ $rc -eq 0 ]'
check 'pushed: status=pushed' 'grep -qx status=pushed "$dir/output"'
check 'pushed: no warning' '! grep -q "^::warning::" "$dir/stdout"'
check 'pushed: mapped into the layout' 'git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/extra.rs'
run_sync
check 'unchanged: exits 0' '[ $rc -eq 0 ] && grep -qx status=unchanged "$dir/output"'

setup up-to-date
run_sync
check 'up-to-date: exits 0' '[ $rc -eq 0 ] && grep -qx status=up-to-date "$dir/output"'

setup marker
commit_files "$dir/seed" 'marker off upstream' .github/upstream-sha "$(git -C "$dir/seed" rev-parse HEAD)"
git -C "$dir/seed" push -q "$dir/origin.git" main
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
run_sync
check 'marker not on upstream: fails' '[ $rc -ne 0 ] && grep -q "is not on upstream main" "$dir/stderr"'

setup fetch
run_sync "$dir/missing"
check 'upstream fetch error: fails' '[ $rc -ne 0 ]'

setup push
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
printf '#!/bin/sh\nexit 1\n' >"$dir/origin.git/hooks/pre-receive"
chmod +x "$dir/origin.git/hooks/pre-receive"
run_sync
check 'refused push: fails' '[ $rc -ne 0 ] && ! grep -q "^status=pushed" "$dir/output"'

setup error
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
run_sync "$dir/upstream" "$dir/missing/body.md"
check 'script error while blocked: fails' '[ $rc -ne 0 ] && ! grep -q "^::warning::" "$dir/stdout"'

skip() {
	commit_files "$dir/seed" 'skip list' .github/upstream-skip "$1"
	git -C "$dir/seed" push -q "$dir/origin.git" main
}
marker_on_branch() { [ "$(git -C "$dir/origin.git" show sync/upstream:.github/upstream-sha)" = "$1" ]; }

setup skip
commit_files "$dir/upstream" 'chore: bump' src/main.rs 'fn main() { voxl::run(); }'
skipped=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
skip "$skipped release bump"
run_sync
check 'skip: exits 0' '[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output"'
check 'skip: later commit applied' 'git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/extra.rs'
check 'skip: listed commit left out' \
	'[ "$(git -C "$dir/origin.git" show sync/upstream:crates/starfold-server/src/main.rs)" = "fn main() { starfold::run() }" ]'
check 'skip: marker past both' 'marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)"'
check 'skip: body lists it with the reason' \
	'grep -qF "Skipped (1), listed in \`.github/upstream-skip\`:" "$dir/body.md" && grep -qF -- "- ${skipped:0:7} chore: bump: release bump" "$dir/body.md"'

setup skip-only
commit_files "$dir/upstream" 'chore: bump' src/main.rs 'fn main() { voxl::run(); }'
skipped=$(git -C "$dir/upstream" rev-parse HEAD)
skip "$skipped release bump"
run_sync
check 'skip only: pushed, marker advanced' '[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && marker_on_branch "$skipped"'

setup skip-then-stop
commit_files "$dir/upstream" 'chore: bump' README.md 'voxl 2'
skipped=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
skip "$skipped release bump"
run_sync
check 'skip then stop: pushed, marker at the skipped one' '[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && marker_on_branch "$skipped"'
check 'skip then stop: body names the stop' 'grep -qF "fix: run**: conflicts in crates/starfold-server/src/main.rs. Still to sync (1)" "$dir/body.md"'

setup skip-malformed
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
skip "$(git -C "$dir/upstream" rev-parse --short HEAD) short sha"
run_sync
check 'malformed skip list: fails' '[ $rc -ne 0 ] && grep -q "upstream-skip: each line needs" "$dir/stderr"'

setup held
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
held=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: more' src/more.rs 'pub fn more() {}'
skip "$held held: until its review"
run_sync
check 'held first: exits 0, status=held' '[ $rc -eq 0 ] && grep -qx status=held "$dir/output"'
check 'held first: one warning naming it' \
	'[ "$(grep -c "^::warning::" "$dir/stdout")" = 1 ] && grep -qF "::warning::upstream sync is held at ${held:0:7} feat: extra: until its review" "$dir/stdout"'
check 'held first: summary names it' 'grep -qF "held in \`.github/upstream-skip\`: until its review. Still to sync (2)" "$dir/summary"'
check 'held first: nothing pushed' '! has_branch'

setup applied-then-held
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
before=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: more' src/more.rs 'pub fn more() {}'
skip "$(git -C "$dir/upstream" rev-parse HEAD) held: until its review"
SYNC_MAX_COMMITS=2 run_sync
check 'applied then held: pushed, marker before the held one' \
	'[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && marker_on_branch "$before"'
check 'applied then held: held commit not applied' '! git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/more.rs'
check 'applied then held: body says it waits' 'grep -qF "wait until its line is removed" "$dir/body.md"'

setup held-empty
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
skip "$(git -C "$dir/upstream" rev-parse HEAD) held: "
run_sync
check 'held without a follow-up: fails' '[ $rc -ne 0 ] && grep -q "upstream-skip: each line needs" "$dir/stderr"'
setup hook
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
printf '#!/bin/sh\nexit 1\n' >"$dir/work/.git/hooks/prepare-commit-msg"
chmod +x "$dir/work/.git/hooks/prepare-commit-msg"
run_sync
check 'failing commit hook: fails, not blocked' \
	'[ $rc -ne 0 ] && ! grep -q "^status=blocked" "$dir/output" && ! grep -q "^::warning::" "$dir/stdout" && grep -q "failed without a conflict" "$dir/stderr" && ! has_branch'

fail_git() {
	mkdir -p "$dir/bin"
	printf '#!/bin/sh\n[ "$1" = %s ] && exit 128\nexec %s "$@"\n' "$1" "$(command -v git)" >"$dir/bin/git"
	chmod +x "$dir/bin/git"
	PATH=$dir/bin:$PATH run_sync
}
red() { [ $rc -ne 0 ] && ! grep -q "^status=" "$dir/output" && ! grep -q "^::warning::" "$dir/stdout" && ! has_branch; }

setup ls-remote
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
fail_git ls-remote
check 'ls-remote error: fails, not blocked' 'red && grep -q "git ls-remote exit 128" "$dir/stderr"'

setup merge-base
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
fail_git merge-base
check 'merge-base error: fails, not "not on upstream"' \
	'red && grep -q "merge-base --is-ancestor .* failed (exit 128)" "$dir/stderr" && ! grep -q "is not on upstream main" "$dir/stderr"'

setup rev-list
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
fail_git rev-list
check 'rev-list error: fails, not up-to-date' 'red'

setup ls-tree
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
fail_git ls-tree
check 'tree mapping error: fails, not blocked' 'red && grep -q "mapping the tree of" "$dir/stderr"'

setup diff
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
fail_git diff
check 'changed-files error: fails, nothing pushed' 'red'

setup cap
commit_files "$dir/upstream" 'feat: one' src/one.rs 'pub fn one() {}'
first=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'chore: bump' README.md 'voxl 2'
skip "$(git -C "$dir/upstream" rev-parse HEAD) release bump"
commit_files "$dir/upstream" 'feat: two' src/two.rs 'pub fn two() {}'
run_sync
check 'cap default: one commit applied' \
	'[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && marker_on_branch "$first" && ! git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/two.rs 2>/dev/null'
check 'cap default: body lists the rest' 'grep -qF "At most 1 applied per run; still to sync (2), oldest first:" "$dir/body.md"'
SYNC_MAX_COMMITS=2 run_sync
check 'cap 2: both applied, the skip between them not counted' \
	'[ $rc -eq 0 ] && marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)" && git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/two.rs && ! grep -q "At most" "$dir/body.md"'
SYNC_MAX_COMMITS=0 run_sync
check 'cap 0: fails' '[ $rc -ne 0 ] && grep -q "SYNC_MAX_COMMITS must be a positive integer" "$dir/stderr"'

setup cap-skip-first
commit_files "$dir/upstream" 'chore: bump' README.md 'voxl 2'
skip "$(git -C "$dir/upstream" rev-parse HEAD) release bump"
commit_files "$dir/upstream" 'feat: one' src/one.rs 'pub fn one() {}'
run_sync
check 'cap default: a leading skip does not use it up' \
	'[ $rc -eq 0 ] && marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)" && git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/one.rs'

[ $failures -eq 0 ]
