# Releasing Waku

Waku ships signed macOS (Apple Silicon) builds and in-app updates through
**GitHub Actions and GitHub Releases**, with
[Sparkle](https://sparkle-project.org) installing the updates. R2 remains an
optional download mirror for the website; in-app updates do not depend on it.

Once set up, cutting a release is:

```sh
git tag v<version>
git push origin v<version>
```

In Claude Code, `/release` runs the whole procedure — version bump, changelog,
tag, CI watch, and feed verification — from
[`.claude/skills/release/SKILL.md`](.claude/skills/release/SKILL.md).

- Updater code: [`src/updater.rs`](src/updater.rs) — loads the embedded
  Sparkle.framework. An **Install update** icon appears at the top-right of the
  sidebar when an update is available; activating it downloads/installs the
  update and relaunches Waku. Enter and Space activate the same control.
  **Check for Updates…** lives in the app menu.
- Feed URL + public key: [`resources/Info.plist`](resources/Info.plist)
  (`SUFeedURL`, `SUPublicEDKey`).
- Framework embedding + pinned Sparkle version:
  [`scripts/bundle.sh`](scripts/bundle.sh) (bump `sparkle_version` and
  `sparkle_sha256` together; the distribution is cached under
  `.waku-cache/sparkle/`).
- Release automation: [`scripts/release.ts`](scripts/release.ts),
  [`scripts/appcast.ts`](scripts/appcast.ts),
  [`scripts/changelog.ts`](scripts/changelog.ts),
  [`scripts/verify-update-feeds.ts`](scripts/verify-update-feeds.ts).
- GitHub Actions: [`.github/workflows/release.yml`](.github/workflows/release.yml)
  builds, notarizes, and publishes the release;
  [`.github/workflows/sync-release.yml`](.github/workflows/sync-release.yml)
  copies published assets into the R2 bucket.

---

## One-time setup

CI needs the repository secrets listed under
[Repository secrets](#repository-secrets). For notarization, it accepts either
an App Store Connect API key or an Apple ID and app-specific password. The
signing identity is derived from the imported Developer ID Application
certificate unless `WAKU_SIGNING_IDENTITY` overrides it. An unencrypted `.p12`
needs no `APPLE_CERTIFICATE_PASSWORD` secret.

For local macOS packaging, the release script runs on [Bun](https://bun.sh)
and needs [`create-dmg`](https://github.com/create-dmg/create-dmg) and
[rclone](https://rclone.org) (`brew install bun create-dmg rclone`).

### 1. Sparkle signing keys

Updates are signed with an ed25519 key; the private half stays in the login
keychain and the public half ships in Info.plist as `SUPublicEDKey`.

Waku uses its own key in the **`sh.waku.updates`** login Keychain account.
`scripts/appcast.ts` selects that account for local releases. CI instead reads
the matching exported key from the `SPARKLE_PRIVATE_KEY` repository secret.
The public key in Info.plist must match this dedicated Waku key.

On a fresh machine, restore the key from the password-manager backup with the
Sparkle tools (they land in `.waku-cache/sparkle/<version>/bin` after any
build, or download the release from
[sparkle-project/Sparkle](https://github.com/sparkle-project/Sparkle/releases)):

```sh
./bin/generate_keys --account sh.waku.updates -f sparkle_private_key.txt
./bin/generate_keys --account sh.waku.updates -p  # must match SUPublicEDKey
```

> ⚠️ Lose the private key and existing installs can never update again. Keep
> the backup current.

To configure CI from this Mac, export the dedicated account to a temporary
local file and upload it directly to GitHub without printing it:

```sh
(
  set -eu
  umask 077
  sparkle_tmp=$(mktemp -d)
  trap 'rm -rf "$sparkle_tmp"' EXIT
  .waku-cache/sparkle/2.9.4/bin/generate_keys --account sh.waku.updates \
    -x "$sparkle_tmp/private-key"
  gh secret set SPARKLE_PRIVATE_KEY --repo limboy/raku \
    < "$sparkle_tmp/private-key"
)
```

Sparkle can rotate the EdDSA key as long as the update keeps the same
Developer ID signing identity. Never change both identities in the same
release.

### 2. Developer ID signing + notarization

Copy `.env.example` to `.env` and replace the signing placeholder. Configure
both analytics values to enable analytics, or leave both unset. Bun loads
these values before Cargo compiles the release, so the analytics endpoint and
website ID are embedded in the executable. The script notarizes with the
`NOTARY` keychain profile by default. On a fresh machine:

```sh
cp .env.example .env
xcrun notarytool store-credentials NOTARY \
  --apple-id you@example.com --team-id YOUR_APPLE_TEAM_ID
```

Override the environment with `--signing-identity`, or change the notary
profile with `--notary-profile` / `WAKU_NOTARY_PROFILE`.

### 3. Cloudflare R2 mirror (optional)

1. Create the bucket **`waku-releases`** (Cloudflare dashboard → R2 → Create
   bucket). The release script will not create it — a bucket-scoped API token
   can't.
2. Attach the custom domain **`releases.waku.sh`** to the bucket (bucket →
   Settings → Custom Domains). This serves objects publicly at
   `https://releases.waku.sh/<file>`.
3. Make sure the R2 API token behind the `r2` rclone remote covers this bucket
   (R2 → Manage API Tokens → Object Read & Write). The remote already exists
   for kero; if `rclone lsf r2:waku-releases --s3-no-check-bucket` returns
   *AccessDenied* after the bucket exists, extend the token's bucket list.

The rclone remote itself (`~/.config/rclone/rclone.conf`, type S3, provider
Cloudflare, `no_check_bucket = true`) is shared with kero and needs no change.
For CI mirroring, add the `R2_*` repository secrets.

---

## Cutting a release

1. **Bump `version` in the root `Cargo.toml`** — the single source of truth —
   and run `cargo update --workspace --offline` so `Cargo.lock` follows.
   `CFBundleShortVersionString` is the version, and `CFBundleVersion` is
   derived from it (`major*1e6 + minor*1e3 + patch`, so `0.2.0` → `2000`),
   which keeps Sparkle's build-number comparison monotonic without a manual
   counter. Prerelease versions (`-beta.1`) are refused — the appcast serves
   one stable channel.
2. **Write the release notes** — add a `## [<version>] - <date>` section below
   `## [unreleased]` in [`CHANGELOG.md`](CHANGELOG.md). Cover everything since
   the last *published* release: a tag whose run never published reached no
   users, so its changes belong in the next section.
3. **Commit, tag, and push:**
   ```sh
   git commit -am "Release Waku <version>"
   git tag v<version>
   git push origin main
   git push origin v<version>
   ```

The Release workflow cross-compiles the Apple-silicon binaries on an Intel
macOS runner, signs the app and update archive, notarizes and staples the DMG,
generates the appcast, and publishes the GitHub release with the matching
changelog section. Installed clients can discover the update as soon as
publication completes. The feed and its downloads are release assets:

- **Feed**: `https://github.com/limboy/raku/releases/latest/download/appcast.xml`
- **Archive and release notes**: immutable versioned URLs under
  `https://github.com/limboy/raku/releases/download/v<version>/`.

Before publishing, CI verifies that the feed references the matching release
assets and that the declared length and Ed25519 signature match the actual
archive and the app's public key. A missing asset, unsigned item, wrong key,
or incorrect URL fails the job. Publishing marks the release as latest, moving
the stable feed URL only after every asset has been uploaded. A version that is
already published is never rebuilt.

Test by keeping an older build around, launching it, and choosing
**Check for Updates…**.

### Ways to run the workflow

- **Push a `v*` tag** (preferred) — the tag must match the `version` in
  `Cargo.toml`, or the run fails before anything builds.
- **Actions → Release → Run workflow** — no tag needed. The run releases
  whatever `Cargo.toml` says as `v<version>` and creates that tag at the built
  commit when it publishes. The optional `source_ref` input builds a different
  ref than the one the workflow runs from.

Prefer tag pushes. A manual run whose workflow ref differs from the commit
being released — for example, run from a newer `main` to publish an existing
older tag — fails at **Publish GitHub release** with
`403 Resource not accessible by integration`, even though the job's token has
`contents: write`. The signed artifacts are still uploaded to the run, so they
can be published by hand:

```sh
D=$(mktemp -d)
gh run download <run-id> -R limboy/raku -n macos -D "$D"
gh release create v<version> -R limboy/raku --verify-tag --latest \
  --title v<version> --notes-file "$D/Waku-<version>.md" \
  "$D/Waku-<version>.dmg" "$D/Waku-<version>.zip" \
  "$D/Waku-<version>.md" "$D/appcast.xml"
```

Stapler validation occasionally fails with `NSURLErrorDomain -1005` /
`Error 68` when the runner loses its connection to Apple's ticket service.
That is transient; rerun the failed jobs.

### Release assets

macOS CI runs `bun run release --local --skip-build` after building the
binaries, and publishes:

- `Waku-<version>.dmg` — what people download
- `Waku-<version>.zip` — what Sparkle installs
- `Waku-<version>.md` — release notes shown in the update prompt
- `appcast.xml` — the Sparkle-signed feed

Builds are Apple Silicon only; the appcast declares
`sparkle:hardwareRequirements` `arm64`.

### R2 mirror

**Sync release** copies a published release's assets to the `waku-releases`
R2 bucket, with `appcast.xml` on a short cache lifetime and everything else
cached forever. It skips itself when the R2 credentials are not configured.
A release published with the workflow's `GITHUB_TOKEN` does not emit release
events, so the publish job starts Sync release explicitly. You can also run
**Sync release** manually from Actions with a tag.

### Repository secrets

| Secret | Purpose |
| --- | --- |
| `WAKU_ANALYTICS_ENDPOINT` | optional with `WAKU_ANALYTICS_WEBSITE_ID`; analytics endpoint embedded at build time |
| `WAKU_ANALYTICS_WEBSITE_ID` | optional with `WAKU_ANALYTICS_ENDPOINT`; omit both to disable analytics |
| `WAKU_SIGNING_IDENTITY` | optional; overrides the imported Developer ID Application identity |
| `APPLE_CERTIFICATE` | base64-encoded Developer ID Application `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | password for that `.p12`; omit for an empty password |
| `ASC_PRIVATE_KEY` | App Store Connect API key: raw contents of the `.p8` file |
| `ASC_KEY_ID` | API key ID, from `AuthKey_<ID>.p8` |
| `ASC_ISSUER_ID` | issuer UUID for a team API key; omit for an individual key |
| `APPLE_ID` | fallback when no API key is supplied: Apple ID used by `notarytool` |
| `APPLE_APP_SPECIFIC_PASSWORD` | fallback: app-specific password for that Apple ID |
| `APPLE_TEAM_ID` | fallback: Developer Team ID |
| `SPARKLE_PRIVATE_KEY` | EdDSA private key for `generate_appcast` |
| `R2_ACCOUNT_ID` | optional mirror: Cloudflare account id for the R2 API |
| `R2_ACCESS_KEY_ID` | optional mirror: R2 Object Read & Write token |
| `R2_SECRET_ACCESS_KEY` | optional mirror: matching secret |
| `R2_BUCKET` | optional; defaults to `waku-releases` |

### Local macOS packaging

`bun run release --local` builds a signed, notarized DMG, update zip, release
notes, and appcast under `dist/` without publishing. Set
`WAKU_DOWNLOAD_URL_PREFIX=https://github.com/limboy/raku/releases/download/v<version>/`
when preparing assets for GitHub by hand, as CI does.

The bare `bun run release` command retains the legacy R2 publishing path: it
pulls recent archives from R2 to generate binary deltas and uploads there. It
does not publish a GitHub release or advance the feed installed clients
follow.

### Local script options

| Flag / Env | Default | Purpose |
| --- | --- | --- |
| `--local` | — | build, notarize, and write the DMG + zip without publishing |
| `--force` | — | re-publish a version that already exists in R2 |
| `--adhoc`, `--skip-notarize` | — | local test builds (imply `--local`) |
| `--skip-build` | — | reuse existing release binaries |
| `--signing-identity <id>` / `WAKU_SIGNING_IDENTITY` | — | Developer ID identity to sign with |
| `--notary-profile <name>` / `WAKU_NOTARY_PROFILE` | `NOTARY` | `notarytool` keychain profile |
| `--build-number <n>` / `WAKU_BUILD_NUMBER` | derived | `CFBundleVersion` override |
| `WAKU_R2_REMOTE` | `r2` | rclone remote name (legacy R2 path) |
| `WAKU_R2_BUCKET` | `waku-releases` | R2 bucket (legacy R2 path) |
| `WAKU_DOWNLOAD_URL_PREFIX` | `https://releases.waku.sh/` | base URL in the appcast |
| `WAKU_HISTORY_COUNT` | `15` | recent archives pulled for delta generation |
| `WAKU_NO_HISTORY=1` | — | skip pulling old archives (full updates only) |
| `SPARKLE_BIN` | the `.waku-cache` copy | Sparkle tools directory |

---

## Notes

- **Two artifacts per release:** the notarized `.dmg` (what people download)
  and a `.zip` (what Sparkle installs). Only the zip appears in the appcast;
  point download buttons at the DMG. CI feeds carry full updates only; deltas
  come from the legacy R2 path.
- **Keep artifact names stable.** `Waku-<v>.dmg`, `Waku-<v>.zip`, and
  `appcast.xml` are baked into existing clients' feed and download URLs.
- **Debug builds never update themselves.** `Updater::init` returns `None`
  under `debug_assertions`, so the dev watcher's app can't offer to replace
  itself with a production Waku. Set `WAKU_FORCE_UPDATER=1` to exercise the
  real Sparkle flow from a debug bundle anyway. A bare `cargo run` binary has
  no embedded framework and also degrades to no updater. For UI-only testing,
  start the watcher with `WAKU_PREVIEW_UPDATE=1`; the sidebar immediately
  shows an available update and clicking it changes to the spinner without
  installing anything. The preview flag fakes only that sidebar result;
  **Check for Updates…** still uses the embedded Sparkle framework and its
  real standard window.
- **Automatic and explicit checks have separate presentation.** Scheduled
  checks stay silent until the sidebar update button appears. Choosing
  **Check for Updates…** promotes an existing silent result into Sparkle's
  standard updater window, or shows its checking progress while an automatic
  check finishes. With no automatic session active, it starts Sparkle's
  standard user-initiated check directly.
- **First-run consent:** Sparkle shows its one-time "check automatically?"
  prompt on the second launch and persists the user's choice. Explicit
  checks remain available through the app menu.
- **Waku isn't sandboxed**, so Sparkle's XPC services are unnecessary;
  `bundle.sh` strips them (plus headers/modules) from the embedded framework
  and re-signs the rest with the app's identity — hardened-runtime library
  validation requires the identities to match.
