#!/usr/bin/env bash
# Usage: upstream-sync.sh <upstream-url> <body-file>
#
# Rebuilds sync/upstream from origin/main: each upstream main commit after the one in
# .github/upstream-sha (first parent), moved into this repo's layout by map_tree, is
# cherry-picked in order. Stops at the first one that doesn't apply, bumps the marker to
# the last one that did and force-pushes sync/upstream, unless the branch already has
# that content or carries commits the bot didn't make. Writes the pull request body to
# <body-file>, and status=<up-to-date|human-edits|blocked|unchanged|pushed> plus, for
# the last two, head=<sync/upstream sha> to $GITHUB_OUTPUT. When the first new commit
# doesn't apply (blocked), the body also goes to $GITHUB_STEP_SUMMARY with one warning
# naming that commit, and the run still succeeds; any other failure, including any git
# command failing, exits non-zero.
set -euo pipefail
shopt -s inherit_errexit

upstream_url=$1
body=$2
branch=sync/upstream
marker_file=.github/upstream-sha
bot_email=${SYNC_BOT_EMAIL:-41898282+github-actions[bot]@users.noreply.github.com}

output() {
	echo "$1"
	if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "$1" >>"$GITHUB_OUTPUT"; fi
}

# A commit's tree with upstream paths moved to where they live here. Upstream's .github
# (its CI) and package-lock.json (bun.lock here) are left out.
map_tree() {
	local index tree
	index=$(mktemp -u)
	if ! git ls-tree -r -z --full-tree "$1" | perl -0 -ne '
		chomp;
		my ($meta, $path) = split /\t/, $_, 2;
		next if $path =~ m{^\.github/} || $path eq "package-lock.json";
		$path = "crates/starfold-server/$path" if $path =~ m{^src/} || $path eq "build.rs" || $path eq "Cargo.toml";
		$path = "packages/$path" if $path =~ m{^web/};
		print "$meta\t$path\0";
	' | GIT_INDEX_FILE=$index git update-index -z --index-info; then
		echo "mapping the tree of $1 failed" >&2
		rm -f "$index"
		return 1
	fi
	tree=$(GIT_INDEX_FILE=$index git write-tree)
	rm -f "$index"
	echo "$tree"
}

# 0 when $1 is an ancestor of $2, 1 when it isn't; any other git failure exits.
is_ancestor() {
	local rc=0
	git merge-base --is-ancestor "$1" "$2" || rc=$?
	case $rc in 0 | 1) return "$rc" ;; esac
	echo "git merge-base --is-ancestor $1 $2 failed (exit $rc)" >&2
	exit 1
}

line() { git log -1 --format='%h %s' "$1"; }
list() { paste -sd, - | sed 's/,/, /g'; }

git fetch --quiet origin "+refs/heads/main:refs/remotes/origin/main"
git fetch --quiet --no-tags "$upstream_url" "+refs/heads/main:refs/remotes/upstream/main"
remote_branch=
listed=0
git ls-remote --exit-code --heads origin "$branch" >/dev/null || listed=$?
case $listed in
0)
	git fetch --quiet origin "+refs/heads/$branch:refs/remotes/origin/$branch"
	remote_branch=$(git rev-parse "refs/remotes/origin/$branch")
	;;
2) ;;
*)
	echo "listing $branch on origin failed (git ls-remote exit $listed)" >&2
	exit 1
	;;
esac
upstream=$(git rev-parse refs/remotes/upstream/main)
marker=$(git show "refs/remotes/origin/main:$marker_file")
if ! is_ancestor "$marker" "$upstream"; then
	echo "$marker_file ($marker) is not on upstream main ($upstream)" >&2
	exit 1
fi

revs=$(git rev-list --reverse --first-parent "$marker..$upstream")
commits=()
if [ -n "$revs" ]; then mapfile -t commits <<<"$revs"; fi
if [ ${#commits[@]} -eq 0 ]; then
	output status=up-to-date
	exit 0
fi
if [ -n "$remote_branch" ]; then
	committers=$(git log --format=%ce "refs/remotes/origin/main..$remote_branch")
	if [ -n "$committers" ]; then
		others=0
		grep -qvxF "$bot_email" <<<"$committers" || others=$?
		case $others in
		0)
			output status=human-edits
			exit 0
			;;
		1) ;;
		*) exit 1 ;;
		esac
	fi
fi

git checkout --quiet -B "$branch" refs/remotes/origin/main
picked=()
left_out=()
stop=
stop_label=
stop_reason=
for c in "${commits[@]}"; do
	parent_tree=$(map_tree "$c^1")
	base=$(git commit-tree "$parent_tree" -m base)
	tree=$(map_tree "$c")
	author_name=$(git log -1 --format=%an "$c")
	author_email=$(git log -1 --format=%ae "$c")
	author_date=$(git log -1 --format=%ad --date=raw "$c")
	message=$(git log -1 --format=%B "$c")
	mapped=$(GIT_AUTHOR_NAME=$author_name GIT_AUTHOR_EMAIL=$author_email GIT_AUTHOR_DATE=$author_date \
		git commit-tree "$tree" -p "$base" -m "$message" -m "Upstream: tembo/voxl@$c")
	label=$(line "$c")
	if pick_log=$(git cherry-pick --keep-redundant-commits "$mapped" 2>&1); then
		picked+=("$c")
	else
		conflicts=$(git ls-files -u | cut -f2 | sort -u | list)
		if [ -z "$conflicts" ]; then
			echo "cherry-picking $label failed without a conflict:" >&2
			echo "$pick_log" >&2
			exit 1
		fi
		stop=$c
		stop_label=$label
		stop_reason="conflicts in $conflicts"
		git cherry-pick --abort
		break
	fi
	changed=$(git diff --name-only "$c^1" "$c")
	skipped=$(grep -E '^(\.github/|package-lock\.json$)' <<<"$changed") || [ $? -eq 1 ]
	if [ -n "$skipped" ]; then
		files=$(list <<<"$skipped")
		left_out+=("$label: $files")
	fi
done

if [ ${#picked[@]} -gt 0 ]; then
	last=${picked[${#picked[@]} - 1]}
	echo "$last" >"$marker_file"
	git add "$marker_file"
	git commit --quiet -m "chore: upstream sync through tembo/voxl@${last:0:7}"
fi

{
	echo "Upstream main ($upstream_url) is at \`$upstream\`; main here is synced through \`$marker\` (\`$marker_file\`)."
	echo
	if [ ${#picked[@]} -gt 0 ]; then
		echo "Applied (${#picked[@]}), with upstream paths moved into this repo's layout:"
		for c in "${picked[@]}"; do
			l=$(line "$c")
			echo "- $l"
		done
		echo
	fi
	if [ -n "$stop" ]; then
		rest=("${commits[@]:${#picked[@]}}")
		echo "**Stopped at $stop_label**: $stop_reason. Still to sync (${#rest[@]}), oldest first:"
		for c in "${rest[@]}"; do
			l=$(line "$c")
			echo "- $l"
		done
		echo
		echo "To go on by hand, on \`$branch\` (branched from main if it doesn't exist): apply \`$stop\` with \`src/\`, \`build.rs\` and \`Cargo.toml\` under \`crates/starfold-server/\` and \`web/\` under \`packages/web/\`, resolve, write its sha to \`$marker_file\`, commit and push. The bot leaves the branch alone while it has commits the bot didn't make."
		echo
	fi
	if [ ${#left_out[@]} -gt 0 ]; then
		echo "Left out of the applied commits (upstream's CI and npm lockfile; port by hand if wanted):"
		for l in "${left_out[@]}"; do echo "- $l"; done
		echo
	fi
} >"$body"

if [ ${#picked[@]} -eq 0 ]; then
	output status=blocked
	if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$body" >>"$GITHUB_STEP_SUMMARY"; fi
	warning="upstream sync is blocked at $stop_label, which doesn't apply here; see the job summary"
	warning=${warning//%/%25}
	warning=${warning//$'\r'/%0D}
	echo "::warning::${warning//$'\n'/%0A}"
	exit 0
fi
if [ -n "$remote_branch" ] && is_ancestor refs/remotes/origin/main "$remote_branch"; then
	remote_tree=$(git rev-parse "$remote_branch^{tree}")
	head_tree=$(git rev-parse "HEAD^{tree}")
	if [ "$remote_tree" = "$head_tree" ]; then
		output "head=$remote_branch"
		output status=unchanged
		exit 0
	fi
fi
git push --quiet --force-with-lease="refs/heads/$branch:$remote_branch" origin "HEAD:refs/heads/$branch"
head=$(git rev-parse HEAD)
output "head=$head"
output status=pushed
