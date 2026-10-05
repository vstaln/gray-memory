//! The `gray memory <args>` surface, ported from the deleted
//! `crates/gray/src/memory.rs` CLI block. Reached two ways: `gray memory …`
//! (the host `exec`s this binary with the args) and `/memory …` inside a
//! session (the host routes argv over `command/run`). Both land here;
//! [`run`] returns the text instead of printing so the sidecar can wrap it.

use clap::{Args, Subcommand};

use anyhow::ensure;

use crate::store::{MemoryStore, Scope};

#[derive(Args, Debug, Clone)]
pub struct MemoryArgs {
    /// User preferences or current project's confirmed decisions
    #[arg(long, value_enum, default_value = "project", global = true)]
    pub scope: Scope,
    /// Show each entry's save date and originating session
    #[arg(long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: MemoryCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MemoryCommand {
    /// Show the current curated entries
    List,
    /// Show one entry
    Show { key: String },
    /// Add or replace a named entry (one line; never credentials)
    Set { key: String, text: String },
    /// Rewrite an existing entry's text (fails when the key is unknown)
    Edit { key: String, text: String },
    /// Forget a named entry for future sessions
    Remove { key: String },
    /// Forget every entry in the scope
    Clear,
    /// Review entries against the keep/delete rule (advisory; deletes nothing)
    Audit,
    /// Daily-ingest add: refuses an existing key (the ingest never overwrites)
    IngestSet { key: String, text: String },
    /// Daily-ingest reconcile: the new text must keep the old one verbatim
    IngestEdit { key: String, text: String },
}

/// The hard kill-switch (`GRAY_NO_MEMORY`), same shape as core's
/// `memory::disabled`: gates saves; the served path gates on
/// [`crate::enabled`] too.
pub fn disabled() -> bool {
    std::env::var("GRAY_NO_MEMORY").is_ok_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

/// `run_cli`, returning the lines instead of printing so `command/run` can
/// answer with `{text}`.
pub fn run(args: &MemoryArgs) -> anyhow::Result<String> {
    run_at(args, &crate::gray_home()?, &std::env::current_dir()?)
}

/// Testable seam: the same dispatch against explicit home + cwd.
pub fn run_at(
    args: &MemoryArgs,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> anyhow::Result<String> {
    let store = MemoryStore::new(home, cwd)?;
    let mut out = String::new();
    match &args.command {
        MemoryCommand::List => {
            let text = if args.verbose {
                store.list_detailed(args.scope)?
            } else {
                store.list(args.scope)?
            };
            if text.trim().is_empty() {
                out.push_str("No memories.\n");
            } else {
                out.push_str(&text);
            }
        }
        MemoryCommand::Show { key } => match store.get(args.scope, key)? {
            Some(text) => out.push_str(&format!("- {key}: {text}\n")),
            None => out.push_str(&format!("No entry named '{key}'.\n")),
        },
        MemoryCommand::Set { key, text } => {
            ensure!(!disabled(), "memory saving disabled by GRAY_NO_MEMORY");
            let changed = store.set(args.scope, key, text)?;
            out.push_str(&format!(
                "{}\n",
                if changed {
                    "Memory updated."
                } else {
                    "Memory unchanged."
                }
            ));
            warn_on_growth(&mut out, &store, args.scope, changed);
        }
        MemoryCommand::Edit { key, text } => {
            ensure!(!disabled(), "memory saving disabled by GRAY_NO_MEMORY");
            let changed = store.edit(args.scope, key, text)?;
            out.push_str("Memory edited.\n");
            warn_on_growth(&mut out, &store, args.scope, changed);
        }
        MemoryCommand::Remove { key } => {
            store.remove(args.scope, key)?;
            out.push_str(
                "Memory removed. Existing sessions and transcripts retain their earlier context.\n",
            );
        }
        MemoryCommand::Clear => {
            ensure!(!disabled(), "memory saving disabled by GRAY_NO_MEMORY");
            let dropped = store.clear(args.scope)?;
            out.push_str(&format!(
                "{}\n",
                match dropped {
                    0 => "No memories to clear.".to_string(),
                    1 => "Memory cleared (1 entry).".to_string(),
                    n => format!("Memory cleared ({n} entries)."),
                }
            ));
        }
        MemoryCommand::Audit => {
            out.push_str(&store.audit(args.scope)?);
        }
        MemoryCommand::IngestSet { key, text } => {
            ensure!(!disabled(), "memory saving disabled by GRAY_NO_MEMORY");
            let changed = store.ingest_set(args.scope, key, text)?;
            out.push_str(&format!(
                "{}\n",
                if changed {
                    "Memory added."
                } else {
                    "Memory unchanged."
                }
            ));
            warn_on_growth(&mut out, &store, args.scope, changed);
        }
        MemoryCommand::IngestEdit { key, text } => {
            ensure!(!disabled(), "memory saving disabled by GRAY_NO_MEMORY");
            let changed = store.ingest_edit(args.scope, key, text)?;
            out.push_str(&format!(
                "{}\n",
                if changed {
                    "Memory reconciled (both claims kept)."
                } else {
                    "Memory unchanged."
                }
            ));
            warn_on_growth(&mut out, &store, args.scope, changed);
        }
    }
    Ok(out)
}

/// The ratchet warning, appended after a save that extends a growth streak.
fn warn_on_growth(out: &mut String, store: &MemoryStore, scope: Scope, changed: bool) {
    if changed && let Some(warning) = store.growth_warning(scope) {
        out.push_str(&warning);
        out.push('\n');
    }
}
