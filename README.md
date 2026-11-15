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

One binary, four surfaces:

- **`memory` tool** (`tool/call`) — the immediate write lane. `remember`
  lands on `ingest_set`, `amend` on `ingest_edit`; there is no
  `replace`/`remove`/`clear` in the tool's vocabulary, so the model can
  only ever add flagged, auditable entries.
- **manual distill** (`gray-memory distill-run --session SID --cwd DIR
  [--model M]`) — an on-demand ingest pass: reads the transcript tail via
  `gray read`, asks a `--bare --lean` `gray -p` worker what is worth
  keeping, and executes *only* the `ingest-set`/`ingest-edit` lines it
  emits — the worker's `GRAY_MEMORY_HOME` is a throwaway dir and
  `GRAY_NO_MEMORY=1` is set, so a poisoned transcript can at worst leave a
  flagged entry, never wipe. Nothing invokes it automatically (the
  turn_end ticker was removed — writes happen inline via the `memory`
  tool). `gray memory distill-log` tails `<home>/memory/distill.log`; the
  bare `/memory` status line reports the last run.

- **`prompt/context`** serves an index, not the entries: one `key: hint`
  line (about 8 words) per entry, ranked by relevance to the session's
  project and then by recency, capped at 1 KiB per scope. Full text is one
  `show` away (the `memory` tool or `gray memory show KEY`). The full index
  goes out once per session; the host drops repeats, so later turns cost
  nothing. When entries change mid-session, only a numbered delta
  (`+` added, `~` edited, `-` removed) is appended, so the cached prefix
  stays intact. A new `compaction_boundary` in the session transcript
  triggers a fresh full index.
- **`memory` tool** ops, in the order to prefer them: `remove`, `update`,
  `add`, `show`. `add` refuses when entries with overlapping keys or words
  exist and lists them (`force: true` adds anyway).
- **`/memory …`** runs over `command/run`: `on|off` flips the plugin's own
  marker (`<home>/memory/enabled`, seeded once from the legacy
  `memory_auto` config key); bare reports state; everything else is the
  store CLI below, against the session's cwd.
- **`gray memory …`** is `cli_argv` forwarding — the host `exec`s this
  binary, so the store commands also work with no agent running:

```sh
gray memory [--scope user] set KEY TEXT   # add or replace an entry
gray memory list [--all] [--verbose]      # curated entries (--all: include stale)
gray memory show KEY / edit KEY TEXT / remove KEY / clear
gray memory audit                         # advisory; deletes nothing
gray memory review [--dry-run]            # mark unused (30 days) or broken-path entries stale
gray memory review --keep KEY             # clear a stale mark
gray memory ingest-set / ingest-edit      # the daily-ingest contract
```

The store is `~/.gray/memory/` — `user.md` plus one `project-<hash>.md`
per repo (git root), with `<!-- gray:saved=…;source=… -->` provenance
trailers the model never sees, `snapshots/<session>.json` per-session
index state, `meta.json` (last use and stale marks),
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
  directory under the universal memory home: `GRAY_MEMORY_HOME`, else
  `GRAY_USER_HOME`, else `~/.gray` — never the per-process `GRAY_HOME`, so
  gateway conversations, sandboxes and handoffs all share one store.

---

Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
