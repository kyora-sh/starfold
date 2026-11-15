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
case_n=0
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
	(cd "$dir/work" && GITHUB_OUTPUT=$dir/output GITHUB_STEP_SUMMARY=${SUMMARY:-$dir/summary} "$script" "$url" "$body") \
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
	'grep -qF "Passed over in this run, as \`.github/upstream-skip\` lists them:" "$dir/body.md" && grep -qF -- "- ${skipped:0:7} chore: bump: release bump" "$dir/body.md"'

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
check 'skip then stop: body names the stop' 'grep -qF "fix: run**: conflicts in crates/starfold-server/src/main.rs." "$dir/body.md" && grep -qF "Still to sync (1), oldest first:" "$dir/body.md"'

setup skip-malformed
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
skip "$(git -C "$dir/upstream" rev-parse --short HEAD) short sha"
run_sync
check 'malformed skip list: fails' '[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr"'

setup held
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
held=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: more' src/more.rs 'pub fn more() {}'
skip "$held held: until its review"
run_sync
check 'held first: exits 0, status=held' '[ $rc -eq 0 ] && grep -qx status=held "$dir/output"'
check 'held first: one warning naming it' \
	'[ "$(grep -c "^::warning::" "$dir/stdout")" = 1 ] && grep -qF "::warning::upstream sync is held at ${held:0:7} feat: extra: until its review" "$dir/stdout"'
check 'held first: summary names it' 'grep -qF "held in \`.github/upstream-skip\`: until its review." "$dir/summary" && grep -qF "Still to sync (1), oldest first:" "$dir/summary" && grep -qF "Held (1):" "$dir/summary"'
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
check 'held without a follow-up: fails' '[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr"'

# A skip list made of $1, with S standing for the next upstream commit's sha and U for
# the same in upper case.
list_case() {
	case_n=$((case_n + 1))
	setup "list-$case_n"
	commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
	local sha list
	sha=$(git -C "$dir/upstream" rev-parse HEAD)
	list=${1//S/$sha}
	skip "${list//U/${sha^^}}"
	run_sync
}
for c in $'S held: review\nS release bump' $'S release bump\nS other reason' $'S held: one\nS held: two'; do
	list_case "$c"
	check "duplicate sha (${c//$'\n'/ | }): fails, nothing pushed" \
		'[ $rc -ne 0 ] && grep -q "is listed more than once" "$dir/stderr" && ! has_branch'
done
for c in $'S release bump\r' $'S\theld: review' $'S held:\treview' 'S  release bump' 'S held:review' 'S held:  review' 'S    ' '# a comment'; do
	list_case "$c"
	check "malformed line ${c@Q}: fails, nothing pushed" \
		'[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr" && ! has_branch'
done
list_case $'\nS release bump\n\n'
check 'blank lines around a skip: allowed, the commit is skipped' \
	'[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && grep -qF ": release bump" "$dir/body.md"'
list_case 'S held: review it'
check 'held line with single spaces: held' '[ $rc -eq 0 ] && grep -qx status=held "$dir/output"'
zwsp=$'\xe2\x80\x8b' nbsp=$'\xc2\xa0'
for c in "S ${zwsp}held: review" "S $zwsp" "S${nbsp}release bump" "S release ${zwsp}bump" 'U release bump'; do
	list_case "$c"
	check "non-ASCII or upper-case line ${c@Q}: fails, nothing pushed" \
		'[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr" && ! has_branch'
done
if locale -a 2>/dev/null | grep -qiEx 'en_us\.utf-?8'; then
	LC_ALL=en_US.UTF-8 list_case 'U release bump'
	check 'upper-case sha under en_US.UTF-8: fails, nothing pushed' \
		'[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr" && ! has_branch'
	LC_ALL=en_US.UTF-8 list_case "S ${zwsp}held: review"
	check 'zero-width space before held: under en_US.UTF-8: fails' \
		'[ $rc -ne 0 ] && grep -q "upstream-skip: each line is" "$dir/stderr" && ! has_branch'
else
	echo "note en_US.UTF-8 isn't installed; the locale cases ran under the default locale only"
fi

case_n=$((case_n + 1))
setup "list-$case_n"
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
printf '%s release\0bump\n' "$(git -C "$dir/upstream" rev-parse HEAD)" >"$dir/seed/.github/upstream-skip"
git -C "$dir/seed" add .github/upstream-skip
git -C "$dir/seed" commit -qm 'skip list with a NUL'
git -C "$dir/seed" push -q "$dir/origin.git" main
run_sync
check 'NUL byte in the skip list: fails, nothing pushed' \
	'[ $rc -ne 0 ] && grep -q "upstream-skip contains a NUL byte" "$dir/stderr" && ! has_branch'
setup hook
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
printf '#!/bin/sh\nexit 1\n' >"$dir/work/.git/hooks/prepare-commit-msg"
chmod +x "$dir/work/.git/hooks/prepare-commit-msg"
run_sync
check 'failing commit hook: fails, not blocked' \
	'[ $rc -ne 0 ] && ! grep -q "^status=blocked" "$dir/output" && ! grep -q "^::warning::" "$dir/stdout" && grep -q "failed without a conflict" "$dir/stderr" && ! has_branch'

fail_git() {
	mkdir -p "$dir/bin"
	printf '#!/bin/sh\ncase "$*" in "%s "*) exit 128 ;; esac\nexec %s "$@"\n' "$1" "$(command -v git)" >"$dir/bin/git"
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
fail_git 'ls-tree -r'
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
check 'cap default: body lists the rest' 'grep -qF "At most 1 applied per run." "$dir/body.md" && grep -qF "Still to sync (1), oldest first:" "$dir/body.md" && grep -qF "Skipped (1):" "$dir/body.md"'
SYNC_MAX_COMMITS=2 run_sync
check 'cap 2: both applied, the skip between them not counted' \
	'[ $rc -eq 0 ] && marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)" && git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/two.rs && ! grep -q "At most" "$dir/body.md"'
before=$(git -C "$dir/origin.git" rev-parse sync/upstream)
for n in 0 1000 9223372036854775808 01 -1 ' 1' 1x; do
	SYNC_MAX_COMMITS=$n run_sync
	check "cap ${n@Q}: fails, branch untouched" \
		'[ $rc -ne 0 ] && grep -q "SYNC_MAX_COMMITS must be a whole number from 1 to 999" "$dir/stderr" && [ "$(git -C "$dir/origin.git" rev-parse sync/upstream)" = "$before" ]'
done

setup cap-empty
commit_files "$dir/upstream" 'feat: one' src/one.rs 'pub fn one() {}'
first=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: two' src/two.rs 'pub fn two() {}'
SYNC_MAX_COMMITS='' run_sync
check 'cap empty (a scheduled run): defaults to 1' '[ $rc -eq 0 ] && marker_on_branch "$first"'
setup cap-999
commit_files "$dir/upstream" 'feat: one' src/one.rs 'pub fn one() {}'
commit_files "$dir/upstream" 'feat: two' src/two.rs 'pub fn two() {}'
SYNC_MAX_COMMITS=999 run_sync
check 'cap 999: accepted, applies both' '[ $rc -eq 0 ] && marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)"'

workflow=$(cd "$(dirname "$0")/../workflows" && pwd)/upstream-sync.yml
check 'workflow passes max_commits through with no fallback' \
	'grep -qxF "          SYNC_MAX_COMMITS: \${{ inputs.max_commits }}" "$workflow" && ! grep -q "max_commits ||" "$workflow"'

setup cap-skip-first
commit_files "$dir/upstream" 'chore: bump' README.md 'voxl 2'
skip "$(git -C "$dir/upstream" rev-parse HEAD) release bump"
commit_files "$dir/upstream" 'feat: one' src/one.rs 'pub fn one() {}'
run_sync
check 'cap default: a leading skip does not use it up' \
	'[ $rc -eq 0 ] && marker_on_branch "$(git -C "$dir/upstream" rev-parse HEAD)" && git -C "$dir/origin.git" cat-file -e sync/upstream:crates/starfold-server/src/one.rs'
setup summary
commit_files "$dir/upstream" 'fix: run' src/main.rs 'fn main() { voxl::run(); }'
SUMMARY=$dir/missing/summary.md run_sync
check 'summary write error while blocked: fails, no status, no warning' 'red'

setup left-out
commit_files "$dir/upstream" 'chore: ci and lockfiles' .github/workflows/ci.yml 'on: push' package-lock.json '{}' \
	docs/package-lock.json '{}' .githubx/a 'a' src/extra.rs 'pub fn extra() {}'
run_sync
check 'left out: pushed' '[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output"'
check 'left out: names only upstream CI and the root lockfile' \
	'grep -qF -- ": .github/workflows/ci.yml, package-lock.json" "$dir/body.md" && ! grep -q "docs/package-lock.json\|githubx" "$dir/body.md"'
check 'left out: the rest is applied' \
	'git -C "$dir/origin.git" cat-file -e sync/upstream:docs/package-lock.json && git -C "$dir/origin.git" cat-file -e sync/upstream:.githubx/a && ! git -C "$dir/origin.git" cat-file -e sync/upstream:.github/workflows/ci.yml 2>/dev/null'

setup none-left-out
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
run_sync
check 'nothing left out: no such section' '[ $rc -eq 0 ] && ! grep -q "Left out" "$dir/body.md"'

setup human
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
git -C "$dir/seed" checkout -q -b sync/upstream
commit_files "$dir/seed" 'resolve by hand' README.md 'voxl, by hand'
GIT_COMMITTER_EMAIL=person@example.com git -C "$dir/seed" commit -q --amend --no-edit
git -C "$dir/seed" push -q "$dir/origin.git" sync/upstream
before=$(git -C "$dir/origin.git" rev-parse sync/upstream)
run_sync
check 'human edits: left alone' \
	'[ $rc -eq 0 ] && grep -qx status=human-edits "$dir/output" && [ "$(git -C "$dir/origin.git" rev-parse sync/upstream)" = "$before" ]'

# A sync/upstream holding one commit on main whose committer line is exactly $1.
branch_by() {
	local main tree commit
	main=$(git -C "$dir/seed" rev-parse main)
	tree=$(git -C "$dir/seed" rev-parse "main^{tree}")
	commit=$(printf 'tree %s\nparent %s\nauthor A <a@example.com> 1700000000 +0000\ncommitter %s 1700000000 +0000\n\nby hand\n' \
		"$tree" "$main" "$1" | git -C "$dir/seed" hash-object -t commit -w --literally --stdin)
	git -C "$dir/seed" push -q "$dir/origin.git" "$commit:refs/heads/sync/upstream"
}
for who in 'Person <>' 'Person' 'Person < >' 'bot <BOT@EXAMPLE.COM>'; do
	case_n=$((case_n + 1)); setup "committer-$case_n"
	commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
	branch_by "$who"
	before=$(git -C "$dir/origin.git" rev-parse sync/upstream)
	run_sync
	check "committer \"$who\": human edits, left alone" \
		'[ $rc -eq 0 ] && grep -qx status=human-edits "$dir/output" && [ "$(git -C "$dir/origin.git" rev-parse sync/upstream)" = "$before" ]'
done
setup committer-bot
commit_files "$dir/upstream" 'feat: extra' src/extra.rs 'pub fn extra() {}'
branch_by 'bot <bot@example.com>'
run_sync
check 'committer exactly the bot: synced' '[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output"'

setup quoted
commit_files "$dir/upstream" 'chore: odd names' '.github/a"b' 'x' 'src/c"d.rs' 'pub fn d() {}'
run_sync
check 'quoted names: listed as they are, the rest applied' \
	'[ $rc -eq 0 ] && grep -qF ": .github/a\"b" "$dir/body.md" && git -C "$dir/origin.git" cat-file -e "sync/upstream:crates/starfold-server/src/c\"d.rs"'

# An upstream commit with message $1, kept byte for byte, synced; the synced message is
# then compared with $2 plus the trailer.
message_case() {
	case_n=$((case_n + 1))
	setup "message-$case_n"
	printf 'pub fn m() {}\n' >"$dir/upstream/src/m.rs"
	git -C "$dir/upstream" add -A
	git -C "$dir/upstream" commit -q --cleanup=verbatim -m "$1"
	sha=$(git -C "$dir/upstream" rev-parse HEAD)
	expected="$2"$'\n\n'"Upstream: tembo/voxl@$sha"
	run_sync
}
synced_message() { git -C "$dir/origin.git" log -1 --format=%B sync/upstream~1; }

message_case $'feat: thing (#5)\n\nPlain text (#6), (tembo/voxl#7) and (other/repo#8).\n\nA bare #9 stays.\n\n  <p align="center"><img src="x"></p>\n\nafter the html' \
	$'feat: thing (tembo/voxl#5)\n\nPlain text (tembo/voxl#6), (tembo/voxl#7) and (other/repo#8).\n\nA bare #9 stays.'
check 'message: cut at an HTML block mid-body, refs qualified' '[ $rc -eq 0 ] && [ "$(synced_message)" = "$expected" ]'
check 'message: author kept' \
	'[ "$(git -C "$dir/origin.git" log -1 --format="%an %ae %ad" sync/upstream~1)" = "$(git -C "$dir/upstream" log -1 --format="%an %ae %ad")" ]'
check 'body: applied subject qualified' 'grep -qF -- "- ${sha:0:7} feat: thing (tembo/voxl#5)" "$dir/body.md"'
message_case $'fix: tiny (#10)\n\n<img src="badge">\n<br />' $'fix: tiny (tembo/voxl#10)'
check 'message: only HTML after the subject' '[ $rc -eq 0 ] && [ "$(synced_message)" = "$expected" ]'
message_case $'docs: plain\n\nJust text, (tembo/voxl#3) already qualified.\n  an indented line\n\nEnd.' \
	$'docs: plain\n\nJust text, (tembo/voxl#3) already qualified.\n  an indented line\n\nEnd.'
check 'message: no HTML, kept as is' '[ $rc -eq 0 ] && [ "$(synced_message)" = "$expected" ]'
message_case $'feat: touch (#68)\n\nTested.\n \n  \n\n <br />' $'feat: touch (tembo/voxl#68)\n\nTested.'
check 'message: whitespace-only lines before the HTML dropped' '[ $rc -eq 0 ] && [ "$(synced_message)" = "$expected" ]'
message_case $'\n\n<b>bold</b> subject (#3)\n\nbody text\n<p>x</p>' $'<b>bold</b> subject (tembo/voxl#3)\n\nbody text'
check 'message: leading blank lines skipped, a subject starting with < kept' \
	'[ $rc -eq 0 ] && [ "$(synced_message)" = "$expected" ]'

case_n=$((case_n + 1))
setup "message-$case_n"
printf 'pub fn m() {}\n' >"$dir/upstream/src/m.rs"
{
	printf 'feat: big (#1)\n\nShort text.\n\n'
	printf '<p><img src="https://example.com/badge.svg"></p>\n%.0s' $(seq 40000)
} >"$dir/message"
git -C "$dir/upstream" add -A
git -C "$dir/upstream" commit -q --cleanup=verbatim -F "$dir/message"
sha=$(git -C "$dir/upstream" rev-parse HEAD)
expected=$'feat: big (tembo/voxl#1)\n\nShort text.\n\nUpstream: tembo/voxl@'"$sha"
run_sync
check "message: a $(($(wc -c <"$dir/message") / 1024)) KiB HTML tail is cut, and the sync goes through" \
	'[ $rc -eq 0 ] && grep -qx status=pushed "$dir/output" && [ "$(synced_message)" = "$expected" ]'

case_n=$((case_n + 1))
setup "kinds-$case_n"
commit_files "$dir/upstream" 'feat: a' src/a.rs 'pub fn a() {}'
commit_files "$dir/upstream" 'chore: b (#2)' README.md 'voxl b'
b=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'fix: c' src/c.rs 'pub fn c() {}'
c=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'fix: d' src/d.rs 'pub fn d() {}'
d=$(git -C "$dir/upstream" rev-parse HEAD)
commit_files "$dir/upstream" 'feat: e' src/e.rs 'pub fn e() {}'
e=$(git -C "$dir/upstream" rev-parse HEAD)
skip "$b release bump"$'\n'"$c ported in #46 (abc)"$'\n'"$d held: later"
run_sync
check 'body: still to sync lists only the unmarked commit' \
	'grep -A1 -F "Still to sync (1), oldest first:" "$dir/body.md" | grep -qF -- "- ${e:0:7} feat: e" && [ "$(grep -c -F -- "- ${b:0:7}" "$dir/body.md")" = 1 ]'
check 'body: marked commits listed by kind' \
	'grep -A1 -F "Held (1):" "$dir/body.md" | grep -qF -- "- ${d:0:7} fix: d: later" &&
	grep -A1 -F "Ported (1):" "$dir/body.md" | grep -qF -- "- ${c:0:7} fix: c: ported in #46 (abc)" &&
	grep -A1 -F "Skipped (1):" "$dir/body.md" | grep -qF -- "- ${b:0:7} chore: b (tembo/voxl#2): release bump"'
SYNC_MAX_COMMITS=5 run_sync
check 'body: commits passed over in a run listed by kind' \
	'grep -qF "Passed over in this run" "$dir/body.md" && grep -A1 -F "Ported (1):" "$dir/body.md" | grep -qF -- "- ${c:0:7}" && grep -qF "**Stopped at ${d:0:7} fix: d**: held" "$dir/body.md"'

[ $failures -eq 0 ]
