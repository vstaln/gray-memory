//! `gray-memory` sidecar + CLI, one binary:
//!
//! - `gray-memory manifest` → manifest JSON on stdout (install probe;
//!   answering registers `cli_argv`, so `gray memory …` `exec`s back here)
//! - `gray-memory <memory-args>` → the `gray memory` CLI surface
//! - `gray-memory` (no args, spawned by the host) → wire v1.1 NDJSON loop:
//!   `prompt/context` serves the per-session frozen snapshot and
//!   `command/run` carries `/memory …`.
//!
//! Blocking std I/O, same shape as `gray-questions`; no async runtime.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use clap::Parser;

use gray_memory::cli::{self, MemoryArgs};
use gray_memory::store::MemoryStore;

#[derive(Parser)]
#[command(name = "gray-memory", about = "Curated cross-session memory")]
struct Cli {
    #[command(flatten)]
    args: MemoryArgs,
}

fn manifest() -> serde_json::Value {
    serde_json::json!({
        "name": gray_memory::PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": gray_memory::PROTOCOL,
        "tools": [],
        "commands": gray_memory::COMMANDS,
        "hooks": ["prompt/context"],
        // `gray memory <tab>` vocabulary for the host's completer.
        "completion": ["list", "show", "set", "edit", "remove", "clear",
                       "audit", "ingest-set", "ingest-edit", "on", "off"],
    })
}

/// `prompt/context`: the frozen per-session snapshot under the policy line,
/// or nothing when memory is off (`/memory off`, legacy `memory_auto: false`)
/// or killed (`GRAY_NO_MEMORY`). Empty/absent `session.id` is the anonymous
/// case: a fresh capture that leaves no snapshot file.
fn prompt_context(home: &Path, params: &serde_json::Value) -> serde_json::Value {
    if cli::disabled() || !gray_memory::enabled(home) {
        return serde_json::json!({});
    }
    let session = params["session"]["id"].as_str().unwrap_or("");
    let cwd = params["session"]["cwd"]
        .as_str()
        .or_else(|| params["cwd"].as_str())
        .unwrap_or("");
    let cwd = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        Path::new(cwd).to_path_buf()
    };
    let snapshot = MemoryStore::new(home, &cwd).and_then(|store| {
        store.snapshot(if session.is_empty() {
            None
        } else {
            Some(session)
        })
    });
    match snapshot {
        Ok(text) => serde_json::json!({ "text": gray_memory::served_text(&text) }),
        // Never fail a turn over memory: serve nothing and let the next turn
        // retry.
        Err(_) => serde_json::json!({}),
    }
}

/// `/memory …`: `on|off|enable|disable` flips the plugin marker; bare reports
/// state; anything else runs the store command against the session's cwd.
fn command_run(home: &Path, params: &serde_json::Value) -> serde_json::Value {
    let argv: Vec<String> = params
        .get("argv")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let switch = argv
        .as_slice()
        .first()
        .map(String::as_str)
        .filter(|_| argv.len() == 1);
    if let Some(word) = switch
        && matches!(
            word.to_ascii_lowercase().as_str(),
            "on" | "enable" | "off" | "disable"
        )
    {
        let on = matches!(word.to_ascii_lowercase().as_str(), "on" | "enable");
        return match gray_memory::set_enabled(home, on) {
            Ok(()) => serde_json::json!({ "text": if on {
                "✓ memory on — back in context".to_string()
            } else {
                "✓ memory off — hidden from the model, gray memory still saves".to_string()
            }}),
            Err(e) => serde_json::json!({ "text": format!("memory: {e:#}") }),
        };
    }
    let cwd = params["session"]["cwd"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(Path::new)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if argv.is_empty() {
        let entries = MemoryStore::new(home, &cwd)
            .map(|s| s.entry_count())
            .unwrap_or(0);
        return serde_json::json!({ "text": format!(
            "memory {} — {entries} entries · /memory off hides them from the model (entries keep saving)",
            if gray_memory::enabled(home) { "on" } else { "off" }
        )});
    }
    match Cli::try_parse_from(std::iter::once("memory".to_string()).chain(argv)) {
        Ok(cli) => match cli::run_at(&cli.args, home, &cwd) {
            Ok(text) => serde_json::json!({ "text": text }),
            Err(e) => serde_json::json!({ "text": format!("memory: {e:#}") }),
        },
        Err(e) => serde_json::json!({ "text": e.to_string() }),
    }
}

fn sidecar_loop() -> anyhow::Result<()> {
    let home = gray_memory::gray_home()?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("method").and_then(|m| m.as_str()) == Some("plugin/shutdown") {
            break;
        }
        // Notifications (no id) need no reply.
        let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
            continue;
        };
        let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
        let result = match method {
            "plugin/manifest" => manifest(),
            "prompt/context" => prompt_context(&home, &params),
            "command/run" => command_run(&home, &params),
            _ => continue, // unknown methods/lines are ignored
        };
        let reply = serde_json::json!({ "id": id, "result": result });
        let mut o = stdout.lock();
        writeln!(o, "{reply}")?;
        o.flush()?;
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("manifest") {
        println!("{}", manifest());
        return;
    }
    if args.is_empty() {
        // A TTY human gets the hint; a pipe/host gets the wire loop.
        if std::io::stdin().is_terminal() {
            eprintln!(
                "gray-memory is a plugin sidecar — drive it via `gray memory …` or install with `gray plugin install`"
            );
            return;
        }
        if let Err(e) = sidecar_loop() {
            eprintln!("gray-memory: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    match Cli::try_parse_from(std::iter::once("gray-memory".to_string()).chain(args)) {
        Ok(cli) => match cli::run(&cli.args) {
            Ok(text) => print!("{text}"),
            Err(e) => {
                eprintln!("{e:#}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            // clap prints its own usage for flag errors; exit code is its contract.
            std::process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    }
}
