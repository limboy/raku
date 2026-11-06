---
name: release
description: Cut a Waku release — pick the version, write the CHANGELOG section, bump Cargo.toml, tag and push, watch the Release workflow, recover from known CI failures, and verify the published update feed. Use when the user asks to release, ship, or publish a new Waku version.
argument-hint: "[patch|minor|major|<x.y.z>]"
disable-model-invocation: true
---

# Release Waku

[RELEASING.md](../../../RELEASING.md) is the full reference (secrets, signing,
notarization). This skill is the procedure. Where they disagree, trust
[`.github/workflows/release.yml`](../../../.github/workflows/release.yml): it
currently builds **macOS only**, so the Linux/Windows artifacts described in
RELEASING.md are not produced.

Always pass `-R limboy/raku` to `gh`. This checkout has no default repo, so
bare `gh run …` / `gh release …` fail with "No default remote repository".

A pushed tag publishes to every installed client with no further gate. Get the
user's explicit go-ahead on the version and notes before pushing (step 5).

## 1. Preflight

Stop and report if any check fails.

- Clean tree on `main`: `git status --porcelain` is empty and
  `git branch --show-current` is `main`.
- `git fetch origin --tags`, then `main` must not be behind `origin/main`.
  If it is ahead, those commits ship in this release — list them for the user.
- Last **published** release, not last tag:
  `gh api repos/limboy/raku/releases/latest -q .tag_name`. A tag can exist
  without a release when its run failed or was cancelled (e.g. `v2.1.0`).
  Compare `git tag -l 'v*' --sort=-v:refname | head -5` with
  `gh release list -R limboy/raku -L 5`.
- Something to release: `git log --oneline <published>..HEAD` has commits
  other than `Release Waku …`.
- Tests are green on HEAD:
  `gh run list -R limboy/raku -w test.yml -b main -L 1 --json headSha,status,conclusion`.
  If the latest run is for an older commit or still running, say so and ask
  whether to wait.
- No Release run already in flight:
  `gh run list -R limboy/raku -w release.yml -L 3`.

## 2. Pick the version

- Argument: an explicit `x.y.z`, or `patch` / `minor` / `major` applied to the
  `version` in root `Cargo.toml`. No argument means `patch`.
- It must be stable `major.minor.patch` (the workflow rejects prereleases),
  greater than the current version, and not an existing tag
  (`git rev-parse -q --verify refs/tags/v<x.y.z>` prints nothing).
- Skipping a number left behind by an unpublished tag is fine. Never move or
  reuse a pushed tag.

## 3. Write the CHANGELOG section

- Source the range from the last **published** tag:
  `git log --no-merges --format='%h %s%n%b' <published>..HEAD`, and read diffs
  where a subject is unclear. Changes from an unpublished release never reached
  users, so they belong in this section too (2.1.1 repeats 2.1.0's bullets for
  this reason).
- Follow the rules in the CHANGELOG.md header and match existing entries:
  - Describe user-facing outcomes, not development history.
  - Fold fixes to not-yet-released features into that feature's bullet.
  - Omit internal-only work (CI, refactors, tests, docs) unless it changes
    what users get.
  - One line per bullet, starting with an imperative verb (Add, Improve, Fix,
    Restore, …), no trailing period.
- Insert `## [<x.y.z>] - <YYYY-MM-DD>` (today's local date) directly below
  `## [unreleased]`, moving any bullets already under `[unreleased]` into it.
  Keep the empty `## [unreleased]` heading.
- Check that CI will extract it:

  ```sh
  bun -e 'import { extractReleaseNotes } from "./scripts/changelog.ts";
  console.log(extractReleaseNotes(await Bun.file("CHANGELOG.md").text(), "<x.y.z>"))'
  ```

## 4. Bump the version

- Edit `version` under `[package]` in the root `Cargo.toml` only. The
  `crates/*` packages stay at `0.1.0`.
- `cargo update --workspace --offline` to sync `Cargo.lock`.
- `git diff --stat` touches exactly `CHANGELOG.md`, `Cargo.toml`, and
  `Cargo.lock`, and the lockfile diff is only the `waku` version line.

## 5. Confirm, commit, tag, push

- Show the user the version, the notes, and the commits being released, and
  get an explicit go-ahead through the agent's question tool so Waku shows
  the question sheet: `AskUserQuestion` in Claude Code,
  `request_user_input_async` in Codex. Do not ask in prose, and do not push
  until the answer arrives.
- Commit and tag (existing release tags are lightweight):

  ```sh
  git add CHANGELOG.md Cargo.toml Cargo.lock
  git commit -m "Release Waku <x.y.z>"
  git tag v<x.y.z>
  git push origin main
  git push origin v<x.y.z>
  ```

- Release by **tag push**, not by Actions → Run workflow. A tag push runs the
  workflow file from the tagged commit. A manual run from a newer `main` that
  releases an older commit fails at "Publish GitHub release" with
  `403 Resource not accessible by integration`, even though the token has
  `contents: write` (this happened to v2.1.1).

## 6. Watch the run

- Find the run the tag started (it can take a few seconds to appear):
  `gh run list -R limboy/raku -w release.yml -e push -L 3 --json databaseId,headBranch,status`
  and pick the one whose `headBranch` is `v<x.y.z>`.
- Wait with `gh run watch <id> -R limboy/raku --exit-status` as a background
  Bash command. A run takes about 15–25 minutes, mostly the macOS job. Do not
  poll with sleeps.
- Jobs: **Resolve version** → **macOS** (cross-builds arm64 on an Intel runner,
  signs, notarizes, staples) → **Publish GitHub release**.

## 7. If the run fails

Read `gh run view <id> -R limboy/raku --log-failed | tail -80` and match:

- **Notary or stapler network error** (`NSURLErrorDomain -1005`,
  `The validate action failed! Error 68`, `api.apple-cloudkit.com`): this is
  transient on Apple's side. Rerun with
  `gh run rerun <id> -R limboy/raku --failed`. Rerunning macOS rebuilds and
  re-notarizes, so expect another ~16 minutes.
- **Tag does not match Cargo.toml** in Resolve version: nothing was built or
  published. Ask before deleting the remote tag, then fix the mismatch and
  retag.
- **Publish 403 `Resource not accessible by integration`**: the signed
  artifacts are already in the run. With the user's approval, publish them
  from the user's account:

  ```sh
  D=$(mktemp -d /tmp/waku-v<x.y.z>-XXXX)
  gh run download <id> -R limboy/raku -n macos -D "$D"
  gh release create v<x.y.z> -R limboy/raku --verify-tag --latest \
    --title v<x.y.z> --notes-file "$D/Waku-<x.y.z>.md" \
    "$D/Waku-<x.y.z>.dmg" "$D/Waku-<x.y.z>.zip" "$D/Waku-<x.y.z>.md" "$D/appcast.xml"
  ```

  A manual publish skips the workflow's R2 sync step. If
  `gh secret list -R limboy/raku` shows `R2_*` secrets, start it with
  `gh workflow run sync-release.yml -R limboy/raku --ref main -f tag=v<x.y.z>`.
- **Build, signing, or code failure**: fix it on `main` and release the next
  patch version. Leave the failed tag in place; an unpublished tag is harmless.

## 8. Verify and report

- `gh release view v<x.y.z> -R limboy/raku --json isDraft,assets -q '{isDraft, assets: [.assets[].name]}'`:
  not a draft, with `Waku-<x.y.z>.dmg`, `Waku-<x.y.z>.zip`, `Waku-<x.y.z>.md`,
  and `appcast.xml`.
- `gh api repos/limboy/raku/releases/latest -q .tag_name` is `v<x.y.z>`.
  (`gh release view` has no `isLatest` field.)
- `curl -sL https://github.com/limboy/raku/releases/latest/download/appcast.xml | grep shortVersionString`
  shows `<x.y.z>`. This is the feed Sparkle clients read.
- Report the release URL, the version and notes, the run URL, and anything
  that was skipped or done by hand (manual publish, R2 sync).
