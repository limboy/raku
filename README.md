# Waku

Waku is a fast, native desktop app for working with local coding agents. It is
built in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
and keeps projects, sessions, transcripts on your machine.

## Install

On macOS, [download the signed `.dmg`](https://waku.sh).

On Linux:

```sh
curl -fsSL https://waku.sh/install.sh | sh
```

The script installs into `~/.local` without root. See
[docs/linux.md](docs/linux.md) for requirements, manual installation, and
uninstalling.

On Windows, run `Waku-<version>-<arch>-Setup.exe` from the
[latest release](https://github.com/limboy/waku/releases/latest). It installs
per-user. A portable `.zip` is published alongside it. See
[docs/windows.md](docs/windows.md) for requirements and what is not available
there yet.

## Updates

Release builds check for signed updates in the background. When an update is
available, an **Install update** icon appears at the top of the sidebar. Click
it, or focus it and press Enter or Space, to install and relaunch Waku. Use
**Check for Updates…** in the app menu to check manually.

macOS uses Sparkle; Windows and managed user-local Linux installs verify
updates with the same Ed25519 signing key. Package-manager-owned Linux builds
are updated through their package manager. Development builds keep automatic
updates disabled.

## Supported agents

Waku works with:

- Claude Code
- Codex CLI
- DeepSeek
- Oh My Pi
- OpenCode
- Pi

Install and authenticate at least one supported agent CLI before starting Waku.
Waku detects available CLIs automatically and uses each provider's native
structured protocol and session continuity.

## Highlights

- Keep projects and independent agent sessions in one native app.
- Switch models, reasoning effort, and access modes from a shared interface.
- Queue or steer follow-up messages while an agent is working.
- Rewind Git-backed tasks with conversation-aware checkpoints.
- Export a session as Markdown from its context menu or File menu using
  Export Conversation or Export All (including reasoning and tools).
- Store app state locally, with no Waku account or remote service required.

## Architecture

The native desktop connects to a child `waku-daemon` process over loopback.
Provider sessions run in [`waku-core`](crates/waku-core), behind the
authenticated, versioned WebSocket contract in
[`waku-protocol`](crates/waku-protocol). Waku Desktop depends on
[`waku-client`](crates/waku-client), not on the daemon implementation. The
daemon owns task SQLite data, uploaded attachments, provider-native session
forks, and all workspace filesystem and Git operations; paths returned by it
always refer to the local machine. The desktop retains only presentation state
and a disposable preview cache.

Projectless task workspaces live under `~/.waku/projects/<date>/<slug>`. The
daemon moves workspaces created by the older `~/.waku/<date>/<slug>` layout on
first load.

Configuration ownership is separate too: the Release desktop writes
`~/.waku/app.json`, while Debug stays isolated at `temp/app.json`. Daemon
provider and Computer Use settings live in `~/.waku/settings.json`.

Release apps bundle and sign `waku-daemon`. Development keeps the daemon at
`target/debug/waku-debug-daemon`, allowing provider-only edits to rebuild and
replace the daemon without relaunching Waku Debug.

## Development

Development is supported on macOS, Linux, and Windows and requires
[Rust 1.96 or newer](https://www.rust-lang.org/tools/install) and
[Bun](https://bun.sh/). Linux supports both Wayland and X11, and Windows needs
the MSVC toolchain; install the native build prerequisites listed in
[CONTRIBUTING.md](CONTRIBUTING.md) first.

```sh
bun install
bun run dev
```

The embedded browser and experimental computer-use integration currently
remain macOS-only. Agent sessions, projects, transcripts, skills, usage,
diffs, file editing, and the terminal run natively on Linux and Windows.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and checks.

## Releases

[GitHub Actions](.github/workflows/release.yml) builds macOS, Linux, and Windows
releases when a `v<version>` tag is pushed, or when the Release workflow is run
manually. The tag must match the version in `Cargo.toml`. After every platform
build and update-feed check succeeds, the workflow automatically publishes
the GitHub release and makes the update available to installed clients.

Each release includes signed update archives, `appcast.xml` for macOS, and
architecture-specific appcasts for Linux and Windows. Clients read these
feeds directly from GitHub Releases. CI verifies every feed's download URL,
archive length, and signature before publishing. When R2 credentials are
configured, published assets are also mirrored for the website and install
script.

Release setup uses repository Actions secrets for the Developer ID
certificate, App Store Connect API key, and `SPARKLE_PRIVATE_KEY`. Local macOS
releases use Waku's dedicated `sh.waku.updates` Keychain account. See
[RELEASING.md](RELEASING.md) for the complete secret list, key export commands,
notarization, and release steps.

## Sponsorship

You can support the project development via [GitHub Sponsors](https://github.com/sponsors/egoist).

## License

Waku is licensed under the [GNU General Public License v3.0 only](LICENSE).
