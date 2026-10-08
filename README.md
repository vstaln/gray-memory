<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-memory</h1>
<p align="center">Curated cross-session memory for the gray agent.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-memory/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

Curated cross-session memory for [gray](https://github.com/vstaln/gray),
as a sidecar plugin (wire v1.1). Extracted from gray core — same store,
same bytes, no longer built into the binary.

One binary, three surfaces:

- **`prompt/context`** serves the frozen per-session snapshot (the policy
  line plus non-empty `user`/`decisions` fields as compact JSON). The
  freeze is pinned by `session.id` on the wire, so a session replays
  identical bytes on rebuild — the provider prefix-cache stays warm.
- **`/memory …`** runs over `command/run`: `on|off` flips the plugin's own
  marker (`<home>/memory/enabled`, seeded once from the legacy
  `memory_auto` config key); bare reports state; everything else is the
  store CLI below, against the session's cwd.
- **`gray memory …`** is `cli_argv` forwarding — the host `exec`s this
  binary, so the store commands also work with no agent running:

```sh
gray memory [--scope user] set KEY TEXT   # add or replace an entry
gray memory list [--verbose]              # curated entries (+ provenance)
gray memory show KEY / edit KEY TEXT / remove KEY / clear
gray memory audit                         # advisory; deletes nothing
gray memory ingest-set / ingest-edit      # the daily-ingest contract
```

The store is `~/.gray/memory/` — `user.md` plus one `project-<hash>.md`
per repo (git root), with `<!-- gray:saved=…;source=… -->` provenance
trailers the model never sees, `snapshots/<session>.json` freeze files,
and `GRAY_NO_MEMORY=1` as the hard save kill-switch.

## Install

```sh
cargo install --path .        # or: cargo build --release
gray plugin install ~/.cargo/bin/gray-memory
```

`manifest` answers the CLI probe so registration wires both the sidecar
and `gray memory` forwarding in one shot.

## Notes

- `src/redact.rs` includes third-party MIT-licensed code — see `NOTICE.md`.
- No `host/*` capabilities: the plugin reads and writes only its own
  directory under `GRAY_HOME`.

---

Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
