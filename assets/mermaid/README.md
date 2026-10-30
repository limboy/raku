# Mermaid previews

`pele.global.js` is the unmodified browser distribution of
[Pele](https://github.com/obsidianmd/pele) 1.12.0. Its license is in `LICENSE`.

The native app embeds this file and evaluates it in an isolated QuickJS
runtime on a background worker. Previewing diagrams requires no browser,
network access, or installed Node/Bun runtime. Keeping the distribution in
the repository also lets ordinary Cargo builds work without `node_modules`.

To refresh it, update the pinned development dependency and the version in
`scripts/vendor-pele.ts`, then run `bun install` and `bun run vendor:pele`.
