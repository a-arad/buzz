#!/usr/bin/env bash
# Pre-push guard: CI checks the PR merged with its target, so local runs on a
# skewed branch can pass while CI fails. Block the push only when
# origin/main has changed files this branch also touches.
set -euo pipefail

branch=$(git rev-parse --abbrev-ref HEAD)
if [ "$branch" = "main" ] || [ "$branch" = "HEAD" ]; then
  exit 0
fi

# Fork release PRs can name their actual remote and target branch. Keep the
# same overlap check, and fail closed when that explicitly chosen base is absent.
remote=${CHECK_BRANCH_SKEW_REMOTE:-origin}
target=${CHECK_BRANCH_SKEW_BRANCH:-main}
target_ref="refs/remotes/$remote/$target"
if [[ -n "${CHECK_BRANCH_SKEW_REMOTE:-}${CHECK_BRANCH_SKEW_BRANCH:-}" ]]; then
  git fetch --quiet "$remote" "refs/heads/$target:$target_ref"
  git rev-parse --verify "$target_ref" >/dev/null
else
  git fetch --quiet origin main || true
  git rev-parse --verify --quiet "$target_ref" >/dev/null || exit 0
fi

base=$(git merge-base HEAD "$target_ref")
if [ "$base" = "$(git rev-parse "$target_ref")" ]; then
  exit 0
fi

overlap=$(comm -12 \
  <(git diff --name-only "$base" "$target_ref" -- | sort) \
  <(git diff --name-only "$base" HEAD -- | sort))

if [ -z "$overlap" ]; then
  exit 0
fi

{
  echo "Branch is behind $remote/$target, which changed files this branch also touches:"
  echo "$overlap" | sed 's/^/  /'
  echo "Local checks ran on a tree CI will never test. Run 'git merge $remote/$target',"
  echo "resolve, re-run checks, then push."
} >&2
exit 1
