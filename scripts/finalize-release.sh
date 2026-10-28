#!/usr/bin/env bash
# CI only, gated on every build/test job. Same-run failed-job retries are safe.
set -Eeuo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source "$script_dir/release-version.sh"
fail() { echo "error: $*" >&2; exit 1; }
[[ $# == 4 ]] || fail 'Usage: finalize-release.sh <version> <branch> <tested-sha> <artifact-directory>'
[[ ${GITHUB_ACTIONS:-} == true && ${GITHUB_RUN_ID:-} =~ ^[0-9]+$ ]] || fail 'only run this from the Release workflow'
version=$(release_version "$1")
branch=$2
tested_sha=$3
assets=$(cd "$4" && pwd)
tag="v$version"
git check-ref-format "refs/heads/$branch" >/dev/null
[[ $tested_sha =~ ^[0-9a-f]{40}$ ]] || fail 'expected a full tested commit SHA'
cd "$script_dir/.."
[[ $(git rev-parse HEAD) == "$tested_sha" ]] || fail 'checkout does not match tested source'
[[ -z $(git status --porcelain) ]] || fail 'finalization requires a clean checkout'
command -v jq >/dev/null
archives=()
for platform in "${release_platforms[@]}"; do
  archive=$(release_archive "$version" "$platform")
  [[ -s "$assets/$archive" ]] || fail "missing verified archive for $platform"
  archives+=("$archive")
done
[[ -s "$assets/CHANGELOG.md" && -s "$assets/release-notes.md" ]] || fail 'missing prepared changelog or notes'
(cd "$assets"; shasum -a 256 "${archives[@]}" > SHA256SUMS)
message=$(printf 'Release %s\n\nLocum-Release-Run: %s\nLocum-Release-Source: %s' "$version" "$GITHUB_RUN_ID" "$tested_sha")
remote_tag=$(git ls-remote --tags origin "refs/tags/$tag")
git fetch --no-tags origin "refs/heads/$branch"
remote_head=$(git rev-parse FETCH_HEAD)
if [[ -n $remote_tag ]]; then
  git fetch --no-tags origin "refs/tags/$tag"
  release_commit=$(git rev-parse 'FETCH_HEAD^{commit}')
  [[ $(git cat-file -t FETCH_HEAD) == tag ]] || fail 'existing tag is not annotated'
  [[ $(git show -s --format=%P "$release_commit") == "$tested_sha" ]] || fail 'existing tag has a different source'
  [[ $(git show -s --format=%B "$release_commit") == "$message" ]] || fail 'existing release belongs to another run'
  [[ $(git diff --name-only "$tested_sha" "$release_commit") == CHANGELOG.md ]] || fail 'release changed more than the changelog'
  git show "$release_commit:CHANGELOG.md" | cmp - "$assets/CHANGELOG.md" || fail 'prepared changelog differs'
  git merge-base --is-ancestor "$release_commit" "$remote_head" || fail 'release commit is not on the release branch'
else
  [[ $remote_head == "$tested_sha" ]] || fail 'branch changed during testing; request a new run'
  cp "$assets/CHANGELOG.md" CHANGELOG.md
  git add -- CHANGELOG.md
  git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    commit -m "$message" --only -- CHANGELOG.md
  [[ $(git show -s --format=%P HEAD) == "$tested_sha" ]] || fail 'unexpected release parent'
  [[ $(git diff --name-only "$tested_sha" HEAD) == CHANGELOG.md ]] || fail 'release changed more than the changelog'
  [[ -z $(git status --porcelain) ]] || fail 'commit hook left unexpected changes'
  git -c user.name='github-actions[bot]' -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    tag -a "$tag" -m "$message"
  # Exact old-value lease closes branch races, including rewinds; no history rewrite.
  git push --atomic --force-with-lease="refs/heads/$branch:$tested_sha" origin \
    "HEAD:refs/heads/$branch" "refs/tags/$tag:refs/tags/$tag" || fail 'atomic push rejected; check branch protection/concurrent changes'
fi
marker="<!-- locum-release run=$GITHUB_RUN_ID source=$tested_sha -->"
notes="$assets/publish-notes.md"
cat "$assets/release-notes.md" > "$notes"
printf '\n\n%s\n' "$marker" >> "$notes"
flags=(--prerelease=false)
[[ $version != *-* ]] || flags=(--prerelease --latest=false)
if state=$(gh release view "$tag" --json isDraft,body); then
  [[ $(jq -r .body <<< "$state") == "$(cat "$notes")" ]] || fail 'existing notes do not belong to this run'
  if [[ $(jq -r .isDraft <<< "$state") == false ]]; then
    echo "$tag is already published; nothing was overwritten."
    exit 0
  fi
else
  gh release create "$tag" --verify-tag --draft --notes-file "$notes" --title "locum $version" "${flags[@]}"
fi
uploads=()
for archive in "${archives[@]}"; do uploads+=("$assets/$archive"); done
gh release upload "$tag" "${uploads[@]}" "$assets/SHA256SUMS" --clobber
gh release edit "$tag" --draft=false "${flags[@]}"
