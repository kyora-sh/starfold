#!/usr/bin/env bash
# Usage: upstream-sync.sh <upstream-url> <body-file>
#
# Rebuilds sync/upstream from origin/main: each upstream main commit after the one in
# .github/upstream-sha (first parent), moved into this repo's layout by map_tree, is
# cherry-picked in order; one listed in .github/upstream-skip ("<full sha> <reason>" per
# line) is left out and counts as synced, while one listed as "<full sha> held: <follow-up>"
# is never left out: the sync stops before it. Stops at the first one that is held or
# doesn't apply, or once $SYNC_MAX_COMMITS (default 1) have been applied, bumps the marker to the last one before it and force-pushes
# sync/upstream (never main), unless the branch already has that content or carries
# commits the bot didn't make. Writes the pull request body to <body-file>, and
# status=<up-to-date|human-edits|held|blocked|unchanged|pushed> plus, for the last two,
# head=<sync/upstream sha> to $GITHUB_OUTPUT. When the first commit to sync is held or
# doesn't apply, the body also goes to $GITHUB_STEP_SUMMARY with one warning naming
# that commit, and the run still succeeds; any other failure exits non-zero.
set -euo pipefail

upstream_url=$1
body=$2
branch=sync/upstream
marker_file=.github/upstream-sha
skip_file=.github/upstream-skip
bot_email=${SYNC_BOT_EMAIL:-41898282+github-actions[bot]@users.noreply.github.com}
max_commits=${SYNC_MAX_COMMITS:-1}
if ! [[ $max_commits =~ ^[1-9][0-9]*$ ]]; then
	echo "SYNC_MAX_COMMITS must be a positive integer, not: $max_commits" >&2
	exit 1
fi

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
if ! git merge-base --is-ancestor "$marker" "$upstream"; then
	echo "$marker_file ($marker) is not on upstream main ($upstream)" >&2
	exit 1
fi
skip_list=
if git cat-file -e "refs/remotes/origin/main:$skip_file" 2>/dev/null; then
	skip_list=$(git show "refs/remotes/origin/main:$skip_file")
fi
if bad=$(grep -vE '^[0-9a-f]{40} [^ ]' <<<"$skip_list" | grep -v '^$' || grep -E '^[0-9a-f]{40} held:[[:space:]]*$' <<<"$skip_list"); then
	echo "$skip_file: each line needs a full sha and a reason (or held: and its follow-up), not: $bad" >&2
	exit 1
fi
skip_reason() { awk -v c="$1" '$1 == c { sub(/^[^ ]+ /, ""); print; exit }' <<<"$skip_list"; }

commits=()
while IFS= read -r c; do commits+=("$c"); done < <(git rev-list --reverse --first-parent "$marker..$upstream")
if [ ${#commits[@]} -eq 0 ]; then
	output status=up-to-date
	exit 0
fi
if [ -n "$remote_branch" ]; then
	committers=$(git log --format=%ce "refs/remotes/origin/main..$remote_branch")
	if [ -n "$committers" ] && grep -qvxF "$bot_email" <<<"$committers"; then
		output status=human-edits
		exit 0
	fi
fi

git checkout --quiet -B "$branch" refs/remotes/origin/main
picked=()
skipped_listed=()
left_out=()
last=
handled=0
held_for=
stop=
stop_reason=
for c in "${commits[@]}"; do
	reason=$(skip_reason "$c")
	if [[ $reason == held:* ]]; then
		stop=$c
		held_for=${reason#held:}
		held_for=${held_for# }
		stop_reason="held in \`$skip_file\`: $held_for"
		break
	fi
	if [ -n "$reason" ]; then
		skipped_listed+=("$(line "$c"): $reason")
		last=$c
		handled=$((handled + 1))
		continue
	fi
	base=$(git commit-tree "$(map_tree "$c^1")" -m base)
	mapped=$(
		GIT_AUTHOR_NAME=$(git log -1 --format=%an "$c") \
			GIT_AUTHOR_EMAIL=$(git log -1 --format=%ae "$c") \
			GIT_AUTHOR_DATE=$(git log -1 --format=%ad --date=raw "$c") \
			git commit-tree "$(map_tree "$c")" -p "$base" \
			-m "$(git log -1 --format=%B "$c")" -m "Upstream: tembo/voxl@$c"
	)
	if pick_log=$(git cherry-pick --keep-redundant-commits "$mapped" 2>&1); then
		picked+=("$c")
		last=$c
		handled=$((handled + 1))
	else
		conflicts=$(git ls-files -u | cut -f2 | sort -u | list)
		if [ -z "$conflicts" ]; then
			echo "cherry-picking $(line "$c") failed without a conflict:" >&2
			echo "$pick_log" >&2
			exit 1
		fi
		stop=$c
		stop_reason="conflicts in $conflicts"
		git cherry-pick --abort
		break
	fi
	skipped=$(git diff --name-only "$c^1" "$c" | grep -E '^(\.github/|package-lock\.json$)' || true)
	if [ -n "$skipped" ]; then left_out+=("$(line "$c"): $(echo "$skipped" | list)"); fi
	if [ ${#picked[@]} -eq "$max_commits" ]; then break; fi
done

if [ -n "$last" ]; then
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
	if [ ${#skipped_listed[@]} -gt 0 ]; then
		echo "Skipped (${#skipped_listed[@]}), listed in \`$skip_file\`:"
		for l in "${skipped_listed[@]}"; do echo "- $l"; done
		echo
	fi
	if [ -n "$stop" ]; then
		rest=("${commits[@]:$handled}")
		echo "**Stopped at $(line "$stop")**: $stop_reason. Still to sync (${#rest[@]}), oldest first:"
		for c in "${rest[@]}"; do echo "- $(line "$c")"; done
		echo
		if [ -n "$held_for" ]; then
			echo "It and everything after it wait until its line is removed from \`$skip_file\`."
		else
			echo "To go on by hand, on \`$branch\` (branched from main if it doesn't exist): apply \`$stop\` with \`src/\`, \`build.rs\` and \`Cargo.toml\` under \`crates/starfold-server/\` and \`web/\` under \`packages/web/\`, resolve, write its sha to \`$marker_file\`, commit and push. The bot leaves the branch alone while it has commits the bot didn't make."
		fi
		echo
	elif [ "$handled" -lt ${#commits[@]} ]; then
		rest=("${commits[@]:$handled}")
		echo "At most $max_commits applied per run; still to sync (${#rest[@]}), oldest first:"
		for c in "${rest[@]}"; do echo "- $(line "$c")"; done
		echo
	fi
	if [ ${#left_out[@]} -gt 0 ]; then
		echo "Left out of the applied commits (upstream's CI and npm lockfile; port by hand if wanted):"
		for l in "${left_out[@]}"; do echo "- $l"; done
		echo
	fi
} >"$body"

if [ -z "$last" ]; then
	if [ -n "$held_for" ]; then
		output status=held
		warning="upstream sync is held at $(line "$stop"): $held_for"
	else
		output status=blocked
		warning="upstream sync is blocked at $(line "$stop"), which doesn't apply here; see the job summary"
	fi
	if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$body" >>"$GITHUB_STEP_SUMMARY"; fi
	warning=${warning//%/%25}
	warning=${warning//$'\r'/%0D}
	echo "::warning::${warning//$'\n'/%0A}"
	exit 0
fi
if [ -n "$remote_branch" ] && git merge-base --is-ancestor refs/remotes/origin/main "$remote_branch" &&
	[ "$(git rev-parse "$remote_branch^{tree}")" = "$(git rev-parse "HEAD^{tree}")" ]; then
	output "head=$remote_branch"
	output status=unchanged
	exit 0
fi
git push --quiet --force-with-lease="refs/heads/$branch:$remote_branch" origin "HEAD:refs/heads/$branch"
output "head=$(git rev-parse HEAD)"
output status=pushed
