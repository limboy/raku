# Raku

> Forked from [waku.sh](https://waku.sh) ([egoist/waku](https://github.com/egoist/waku)).

Raku is a fast, native macOS app for working with local coding agents. Built
in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui),
it keeps all projects, sessions, transcripts, and state on your machine.

## Install

Download the latest build from [GitHub Releases](https://github.com/limboy/raku/releases/latest):

- **macOS (Apple Silicon):** Download and open `Raku-<version>.dmg`.

## Updates

Release builds check for signed updates from GitHub Releases in the background.
When an update is available, an **Install update** icon appears at the top of
the sidebar. Click it—or focus it and press Enter or Space—to install and
relaunch Raku. You can also run **Check for Updates…** from the app menu at any
time.

Updates use Sparkle. Development builds keep automatic updates disabled.

## Supported agents

Raku works with:

- Claude Code
- Codex CLI
- Pi

Install and authenticate at least one supported agent CLI before starting Raku.
Raku detects available CLIs automatically and uses each provider's native
structured protocol and session continuity.

## Highlights

- Manage projects and independent agent sessions in one native interface, with pinned sessions, unread completion indicators, and project or date grouping.
- Switch models, reasoning effort, and access modes from a unified composer.
- Queue or steer follow-up messages while an agent is streaming, and trigger native context compaction across providers.
- Inspect token usage and subscription plan quotas (`⌘U`).
- Rewind Git-backed tasks with conversation-aware checkpoints and recover state from automatic daily backups.
- Render rich transcripts with native Mermaid diagrams, LaTeX math, collapsible long-paste snippets, and full Markdown conversation export.
- Keep all data locally on disk with no account or remote service required.

## Architecture

The native desktop connects to a child `raku-daemon` process over loopback.
Provider sessions run in [`raku-core`](crates/raku-core), behind the
authenticated, versioned WebSocket contract in
[`raku-protocol`](crates/raku-protocol). Raku Desktop depends on
[`raku-client`](crates/raku-client), not on the daemon implementation. The
daemon owns task SQLite data, uploaded attachments, provider-native session
forks, and all workspace filesystem and Git operations; paths returned by it
always refer to the local machine. The desktop retains only presentation state
and a disposable preview cache.

Projectless task workspaces live under `~/.raku/projects/<date>/<slug>`. The
daemon moves workspaces created by the older `~/.raku/<date>/<slug>` layout on
first load.

Configuration ownership is separate too: the Release desktop writes
`~/.raku/app.json`, while Debug stays isolated at `temp/app.json`. Daemon
provider settings live in `~/.raku/settings.json`.

Release apps bundle and sign `raku-daemon`. Development keeps the daemon at
`target/debug/raku-debug-daemon`, allowing provider-only edits to rebuild and
replace the daemon without relaunching Raku Debug.

## Development

Development is supported on macOS and requires
[Rust 1.96 or newer](https://www.rust-lang.org/tools/install) and
[Bun](https://bun.sh/). Install the native build prerequisites listed in
[CONTRIBUTING.md](CONTRIBUTING.md) first.

```sh
bun install
bun run dev
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and checks.

## Releases

[GitHub Actions](.github/workflows/release.yml) builds macOS
releases when a `v<version>` tag is pushed, or when the Release workflow is run
manually. The tag must match the version in `Cargo.toml`. After the macOS
build and update-feed check succeed, the workflow automatically publishes the
GitHub release and makes the update available to installed clients.

Each release includes a signed update archive and `appcast.xml`. Clients read
the feed directly from GitHub Releases, and CI verifies its download URL,
archive length, and signature before publishing.

Release setup uses repository Actions secrets for the Developer ID
certificate, App Store Connect API key, and `SPARKLE_PRIVATE_KEY`. Local macOS
releases use Raku's dedicated `sh.waku.updates` Keychain account. See
[RELEASING.md](RELEASING.md) for the complete secret list, key export commands,
notarization, and release steps.

## License

Raku is licensed under the [GNU General Public License v3.0 only](LICENSE).
