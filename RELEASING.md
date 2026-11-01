# Releasing Waku

Waku ships signed in-app updates on macOS, Linux, and Windows through
**GitHub Actions and GitHub Releases**. macOS uses
[Sparkle](https://sparkle-project.org); the native Linux and Windows updaters
read architecture-specific feeds and verify artifacts with the same EdDSA
key. One release workflow produces all platform artifacts and feeds. R2
remains a download mirror for the website and install script; in-app updates
do not depend on that mirror.

Once set up, cutting a release is:

```sh
git tag v<version>
git push origin v<version>
```

- Updater code: [`src/updater.rs`](src/updater.rs) — loads the embedded
  Sparkle.framework on macOS and owns the signed native flows on Linux and
  Windows. An **Install update** icon appears at the top-right of the sidebar
  when an update is available; activating it downloads/installs the update
  and relaunches Waku. Enter and Space activate the same control. **Check for
  Updates…** lives in the app menu.
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
  builds Linux (x86_64, arm64), Windows (x86_64, arm64), and macOS archives on
  a `v*` tag — or on a manual **Run workflow**, which takes the version from
  `Cargo.toml` — and automatically publishes the GitHub release;
  [`.github/workflows/sync-release.yml`](.github/workflows/sync-release.yml)
  copies published assets into the R2 bucket.

---

## One-time setup

CI needs the repository secrets listed below. For notarization, it accepts
either an App Store Connect API key or an Apple ID and app-specific password.
The signing identity is derived from the imported Developer ID Application
certificate unless `WAKU_SIGNING_IDENTITY` overrides it. An unencrypted `.p12`
needs no `APPLE_CERTIFICATE_PASSWORD` secret.

For local macOS packaging,
the release script runs on [Bun](https://bun.sh) and needs
[`create-dmg`](https://github.com/create-dmg/create-dmg) and
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
  gh secret set SPARKLE_PRIVATE_KEY --repo limboy/waku \
    < "$sparkle_tmp/private-key"
)
```

This key replaces the previously shared key. Linux and Windows installations
that trust the old key need one manual upgrade to the first build containing
the new public key. macOS Sparkle can rotate the key if the update retains
the same Developer ID signing identity; do not change both identities in the
same release.

### 2. Developer ID signing + notarization

Copy `.env.example` to `.env` and replace the signing placeholder. Configure
both analytics values to enable analytics, or leave both unset. Bun loads
these values before Cargo compiles the release, so the
analytics endpoint and website ID are embedded in the executable. The script
notarizes with the `NOTARY` keychain profile by default. On a fresh machine:

```sh
cp .env.example .env
xcrun notarytool store-credentials NOTARY \
  --apple-id you@example.com --team-id YOUR_APPLE_TEAM_ID
```

Override the environment with `--signing-identity`, or change the notary
profile with `--notary-profile` / `WAKU_NOTARY_PROFILE`.

### 3. Cloudflare R2 mirror (optional for in-app updates)

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

---

## Cutting a release

1. **Bump `version` in `Cargo.toml`** — the single source of truth.
   `CFBundleShortVersionString` is the version, and `CFBundleVersion` is
   derived from it (`major*1e6 + minor*1e3 + patch`, so `0.2.0` → `2000`),
   which keeps Sparkle's build-number comparison monotonic without a manual
   counter. Prerelease versions (`-beta.1`) are refused for publishing — the
   appcast serves one stable channel.
2. **Write the release notes** — add a `## [<version>]` section at the top of
   [`CHANGELOG.md`](CHANGELOG.md).
3. **Run it:**
   ```sh
   git tag v<version>
   git push origin v<version>
   ```

The Release workflow builds all platforms, signs the update archives,
notarizes the macOS app and DMG, generates the appcasts, and automatically
publishes the GitHub release with the matching changelog section. Installed
clients can discover the update as soon as publication completes. The update
feeds and all their downloads are release assets:

- **macOS feed**: `https://github.com/limboy/waku/releases/latest/download/appcast.xml`
- **Native feeds**: the same URL prefix with `appcast-linux-<arch>.xml` or
  `appcast-windows-<arch>.xml` (`x86_64` and `aarch64`).
- **Archives and release notes**: immutable versioned URLs under
  `https://github.com/limboy/waku/releases/download/v<version>/`.

CI verifies that all five feeds reference the matching release assets and
that their declared lengths and Ed25519 signatures match the actual bytes
and the app's public key. Missing assets, unsigned items, wrong keys, or
incorrect URLs fail the job before publication. Publishing marks
the release as latest, moving the stable feed URLs only after every asset
has been uploaded. A published version is never rebuilt by this workflow.

Test by keeping an older build around, launching it, and choosing
**Check for Updates…**.

### GitHub release assets

The Release workflow runs two ways:

- **Push a `v*` tag** — the tag must match the `version` in `Cargo.toml`, or the
  run fails before anything builds.
- **Actions → Release → Run workflow** — no tag needed. The run releases
  whatever `Cargo.toml` says as `v<version>`; that tag is created at the built
  commit when the workflow publishes the release.

macOS CI runs `bun run release --local`, which signs, notarizes, and writes the
same artifacts as a local release:

- `Waku-<version>.dmg`
- `Waku-<version>.zip`
- `appcast.xml` (Sparkle-signed)

Linux CI adds:

- `waku-<version>-x86_64-unknown-linux-gnu.tar.gz`
- `waku-<version>-aarch64-unknown-linux-gnu.tar.gz`
- `appcast-linux-x86_64.xml`, `appcast-linux-aarch64.xml`
- `latest-linux.txt` — the version `install.sh` resolves "latest" to

Windows CI adds:

- `Waku-<version>-x86_64-Setup.exe`
- `Waku-<version>-aarch64-Setup.exe`
- `waku-<version>-x86_64-pc-windows-msvc.zip` (portable)
- `waku-<version>-aarch64-pc-windows-msvc.zip` (portable)
- `appcast-windows-x86_64.xml`, `appcast-windows-aarch64.xml`
- `latest-windows.txt` — the version the download page resolves "latest" to

[`scripts/bundle-windows.ts`](scripts/bundle-windows.ts) builds both, driving
[`resources/windows/waku.iss`](resources/windows/waku.iss) through Inno Setup's
`ISCC`. The installer is **per-user** (`PrivilegesRequired=lowest`,
`%LOCALAPPDATA%\Programs\Waku`) — no elevation, which is exactly what lets the
updater re-run it silently. The script signs the two executables and the
installer with Authenticode when `WINDOWS_CERTIFICATE` and
`WINDOWS_CERTIFICATE_PASSWORD` are set, and packages them unsigned otherwise,
so a fork without a certificate can still cut a release at the cost of a
SmartScreen warning.

**Never change `AppId` in `waku.iss`.** It is how Windows recognizes an
existing install; a new one turns every update into a second copy in
Add/Remove Programs.

#### The native Windows and Linux update feeds

Windows and Linux have no Sparkle, so [`src/updater.rs`](src/updater.rs) runs
the same contract itself: fetch the appcast, compare versions, download, and
verify the EdDSA signature. Windows hands the installer to Inno Setup with
`/SILENT`. Linux safely unpacks the tarball beside the managed user-local
prefix, then `waku-updater` swaps it after the app's normal quit saves and
rolls back if the replacement cannot open its main window.

- **One feed per architecture.** A Sparkle appcast cannot say which binary an
  item is for, and the client picks its feed at compile time.
- **Same key as macOS.** `build.rs` reads `SUPublicEDKey` out of
  `resources/Info.plist` and compiles it in, so the three platforms cannot
  drift onto different keys.
- [`scripts/appcast-windows.ts`](scripts/appcast-windows.ts) and
  [`scripts/appcast-linux.ts`](scripts/appcast-linux.ts) sign the feeds in the
  publish-release job — the only one holding all native artifacts. They sign
  with Node's Ed25519 over the same `SPARKLE_PRIVATE_KEY`, and refuse to run
  when the key does not derive `SUPublicEDKey` (signing with the wrong key
  ships a feed the app rejects).
- The step pulls the native feeds from the latest GitHub release first and
  merges, so previously published releases keep their entries. macOS CI
  generates a full-update feed for this release without requiring R2 history.

Both Linux jobs run on **Ubuntu 22.04**, and that choice is load-bearing: the
binaries link against the build machine's glibc, so the runner sets the oldest
distribution Waku can start on (2.35 — Ubuntu 22.04, Debian 12, Fedora 36).
Moving those jobs to a newer runner silently drops support for everything
older.

The workflow automatically publishes those files and the matching
`CHANGELOG.md` section as the latest stable GitHub release. Clients read the
feeds directly from GitHub. When R2 credentials are configured, it explicitly
starts the separate Sync release workflow to mirror the assets for the
website and existing mirror consumers.

`appcast.xml`, the architecture-specific Linux/Windows appcasts,
`latest-linux.txt`, and `latest-windows.txt` are the bucket's mutable pointers
and upload with a short cache lifetime; everything else is versioned and
cached forever. Linux users install from that bucket via
[`website/public/install.sh`](website/public/install.sh), served at
`https://waku.sh/install.sh` — see [docs/linux.md](docs/linux.md).

Automatic publication starts **Sync release** when the R2 credentials below
are configured; you can also run **Sync release** manually from Actions.
It uploads the assets to the `waku-releases` R2 bucket. Configure these
repository secrets first:

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
| `WINDOWS_CERTIFICATE` | optional; base64-encoded Authenticode `.pfx` |
| `WINDOWS_CERTIFICATE_PASSWORD` | optional; password for that `.pfx` |
| `R2_ACCOUNT_ID` | Cloudflare account id for the R2 API |
| `R2_ACCESS_KEY_ID` | R2 Object Read & Write token |
| `R2_SECRET_ACCESS_KEY` | matching secret |
| `R2_BUCKET` | optional; defaults to `waku-releases` |

### Local macOS packaging

`bun run release --local` builds a signed, notarized DMG, update zip, release
notes, and appcast under `dist/` without publishing. Set
`WAKU_DOWNLOAD_URL_PREFIX=https://github.com/limboy/waku/releases/download/v<version>/`
when preparing assets for GitHub by hand, as CI does. The bare
`bun run release` command retains the legacy R2 publishing path; it does not
publish a GitHub release or advance the feeds installed clients follow.
That R2 path can pull recent archives to generate binary deltas.

### Local script options

| Flag / Env | Default | Purpose |
| --- | --- | --- |
| `--local` | — | build, notarize, and write the DMG + zip without publishing |
| `--force` | — | re-publish a version that already exists in R2 |
| `--adhoc`, `--skip-notarize` | — | local test builds (imply `--local`) |
| `--skip-build` | — | reuse existing release binaries |
| `--build-number <n>` / `WAKU_BUILD_NUMBER` | derived | `CFBundleVersion` override |
| `WAKU_R2_REMOTE` | `r2` | rclone remote name |
| `WAKU_R2_BUCKET` | `waku-releases` | R2 bucket |
| `WAKU_DOWNLOAD_URL_PREFIX` | `https://releases.waku.sh/` | base URL in the appcast |
| `WAKU_HISTORY_COUNT` | `15` | recent archives pulled for delta generation |
| `WAKU_NO_HISTORY=1` | — | skip pulling old archives (full updates only) |
| `SPARKLE_BIN` | the `.waku-cache` copy | Sparkle tools directory |

---

## Notes

- **Two artifacts per release:** the notarized `.dmg` (what people download)
  and a `.zip` (what Sparkle installs, plus `.delta` files against recent
  builds). Only the zip family appears in the appcast; point download buttons
  at the DMG.
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
- **Old archives stay in R2** so far-behind users can still be served; only
  the recent history is staged locally under `dist/updates/` (git-ignored).
- **Platform artifacts:** keep the bucket layout flat and platform-tagged by
  artifact name/extension — today's macOS names
  (`Waku-<v>.dmg`, `Waku-<v>.zip`, `appcast.xml`) must keep their URLs.
  Linux CI releases produce `waku-<v>-<target>.tar.gz` with
  `scripts/bundle-linux.sh`, Windows CI produces `waku-<v>-<target>.zip` with
  `scripts/bundle-windows.ts`, and both land in GitHub Releases, then R2 via
  the sync workflow. Windows also ships `Waku-<v>-<arch>-Setup.exe`; each
  native client updates from `appcast-<platform>-<arch>.xml` while the Linux
  installer resolves `latest-linux.txt`. `src/updater.rs` is the per-platform
  seam, and everything
  mac-specific in the existing release pipeline lives behind the Darwin guard
  in `scripts/release.ts` plus `scripts/bundle.sh`.
