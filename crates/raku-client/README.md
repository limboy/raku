# raku-client

`raku-client` is the Rust client for `raku-daemon`. It owns the authenticated
WebSocket handshake, request correlation, subscriptions, event sequence
deduplication, replay cursors, local-daemon supervision, and disposable client
preview caches. It depends on `raku-protocol`, never on `raku-core`.

Both bare socket addresses and complete `ws://` or `wss://` URLs are accepted.
Dropping a connection to an externally managed daemon does not stop it.

The local supervisor monitors the process and its connection independently.
When a connection drops while the child is still alive, it reconnects to the
same daemon with replay cursors and preserves buffered events awaiting a
subscription. Provider runtimes survive this recovery. An exited child or a
rebuilt executable still requires a process restart.

Transport errors and recovery attempts are retained in `daemon-connection.log`
beside the task database (`temp/` in Debug; Raku's application data directory in
Release). The log rotates at 256 KiB and retains one previous file. It records
connection diagnostics without request payloads or authentication tokens.
