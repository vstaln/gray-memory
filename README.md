# gray-memory

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

- `src/store.rs` is a verbatim port of the deleted
  `crates/gray/src/memory.rs`; `src/redact.rs` is the disclosure half of
  `crates/gray-core/src/redaction.rs` (itself vendored from
  Hmbown/CodeWhale, MIT — see `THIRD_PARTY_NOTICES.md` in the gray repo).
- No `host/*` capabilities: the plugin reads and writes only its own
  directory under `GRAY_HOME`.
