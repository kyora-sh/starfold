#!/usr/bin/env bash
# Usage: upstream-sync.sh <upstream-url> <body-file>
#
# Rebuilds sync/upstream from origin/main: each upstream main commit after the one in
# .github/upstream-sha (first parent), moved into this repo's layout by map_tree, is
# cherry-picked in order. Stops at the first one that doesn't apply, bumps the marker to
# the last one that did and force-pushes sync/upstream, unless the branch already has
# that content or carries commits the bot didn't make. Writes the pull request body to
# <body-file> and status=<up-to-date|human-edits|blocked|unchanged|pushed> to
# $GITHUB_OUTPUT.
set -euo pipefail

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
	local index
	index=$(mktemp -u)
	git ls-tree -r -z --full-tree "$1" | perl -0 -ne '
		chomp;
		my ($meta, $path) = split /\t/, $_, 2;
		next if $path =~ m{^\.github/} || $path eq "package-lock.json";
		$path = "crates/starfold-server/$path" if $path =~ m{^src/} || $path eq "build.rs" || $path eq "Cargo.toml";
		$path = "packages/$path" if $path =~ m{^web/};
		print "$meta\t$path\0";
	' | GIT_INDEX_FILE=$index git update-index -z --index-info
	GIT_INDEX_FILE=$index git write-tree
	rm -f "$index"
}

line() { git log -1 --format='%h %s' "$1"; }
list() { paste -sd, - | sed 's/,/, /g'; }

git fetch --quiet origin "+refs/heads/main:refs/remotes/origin/main"
git fetch --quiet --no-tags "$upstream_url" "+refs/heads/main:refs/remotes/upstream/main"
remote_branch=
if git ls-remote --exit-code --heads origin "$branch" >/dev/null; then
	git fetch --quiet origin "+refs/heads/$branch:refs/remotes/origin/$branch"
	remote_branch=$(git rev-parse "refs/remotes/origin/$branch")
fi
upstream=$(git rev-parse refs/remotes/upstream/main)
marker=$(git show "refs/remotes/origin/main:$marker_file")
if ! git merge-base --is-ancestor "$marker" "$upstream"; then
	echo "$marker_file ($marker) is not on upstream main ($upstream)" >&2
	exit 1
fi

commits=()
while IFS= read -r c; do commits+=("$c"); done < <(git rev-list --reverse --first-parent "$marker..$upstream")
if [ ${#commits[@]} -eq 0 ]; then
	output status=up-to-date
	exit 0
fi
if [ -n "$remote_branch" ] && git log --format=%ce "refs/remotes/origin/main..$remote_branch" | grep -qvxF "$bot_email"; then
	output status=human-edits
	exit 0
fi

git checkout --quiet -B "$branch" refs/remotes/origin/main
picked=()
left_out=()
stop=
stop_reason=
for c in "${commits[@]}"; do
	base=$(git commit-tree "$(map_tree "$c^1")" -m base)
	mapped=$(
		GIT_AUTHOR_NAME=$(git log -1 --format=%an "$c") \
			GIT_AUTHOR_EMAIL=$(git log -1 --format=%ae "$c") \
			GIT_AUTHOR_DATE=$(git log -1 --format=%ad --date=raw "$c") \
			git commit-tree "$(map_tree "$c")" -p "$base" \
			-m "$(git log -1 --format=%B "$c")" -m "Upstream: tembo/voxl@$c"
	)
	if git cherry-pick --keep-redundant-commits "$mapped" >/dev/null 2>&1; then
		picked+=("$c")
	else
		stop=$c
		stop_reason="conflicts in $(git diff --name-only --diff-filter=U | list)"
		git cherry-pick --abort
		break
	fi
	skipped=$(git diff --name-only "$c^1" "$c" | grep -E '^(\.github/|package-lock\.json$)' || true)
	if [ -n "$skipped" ]; then left_out+=("$(line "$c"): $(echo "$skipped" | list)"); fi
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
		for c in "${picked[@]}"; do echo "- $(line "$c")"; done
		echo
	fi
	if [ -n "$stop" ]; then
		rest=("${commits[@]:${#picked[@]}}")
		echo "**Stopped at $(line "$stop")**: $stop_reason. Still to sync (${#rest[@]}), oldest first:"
		for c in "${rest[@]}"; do echo "- $(line "$c")"; done
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
	exit 0
fi
if [ -n "$remote_branch" ] && git merge-base --is-ancestor refs/remotes/origin/main "$remote_branch" &&
	[ "$(git rev-parse "$remote_branch^{tree}")" = "$(git rev-parse "HEAD^{tree}")" ]; then
	output status=unchanged
	exit 0
fi
git push --quiet --force-with-lease="refs/heads/$branch:$remote_branch" origin "HEAD:refs/heads/$branch"
output status=pushed
