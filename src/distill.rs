//! The cadenced distill — the guaranteed writer (design §5/C).
//!
//! Every `GRAY_MEMORY_DISTILL_EVERY` (default 4) `turn_end`s the sidecar
//! spawns a detached `gray-memory distill-run` subprocess, which: claims a
//! flock for its lifetime, reads the transcript tail via `gray read`, asks
//! a `gray -p` worker what is worth keeping, then executes *only* the
//! `ingest-set`/`ingest-edit` commands the worker emits. The worker's write
//! surface is structural — it emits text, this process parses it, and
//! `set`/`edit`/`remove`/`clear` are not in its vocabulary. The ingest
//! contract (`Why:`/`falsified:`, never-overwrite, daily budgets, secret
//! redaction) does the rest.
//!
//! Running in a detached subprocess (not a sidecar thread) is deliberate:
//! a `gray -p` session ends right after its last `turn_end`, and the
//! sidecar dies with it — a thread would take the parse+execute down with
//! it. The runner holds `distill.lock` (flock) for its own lifetime, which
//! doubles as the liveness check: a dead runner's lock releases itself.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};

use crate::store::{MemoryStore, Scope};

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSCRIPT_WAIT: Duration = Duration::from_secs(12);
const DISTILL_TIMEOUT: Duration = Duration::from_secs(120);
const INGEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FAILURES: u32 = 3;
const TRANSCRIPT_LAST: usize = 40;
const PROMPT_MAX_BYTES: usize = 48 * 1024;
const LOG_MAX_BYTES: u64 = 256 * 1024;
const MAX_COMMANDS: usize = 8;

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Durable run state: `distill.status` + `distill.log` under `<home>/memory/`
// ---------------------------------------------------------------------------

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Status {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    added: usize,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    errors: usize,
    #[serde(default)]
    last_failed: bool,
    #[serde(default)]
    failed_ts: String,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    note: String,
}

fn status_path(root: &Path) -> PathBuf {
    root.join("distill.status")
}

fn read_status(root: &Path) -> Status {
    std::fs::read_to_string(status_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_status(root: &Path, status: &Status) {
    if let Ok(text) = serde_json::to_string(status) {
        let _ = crate::store::atomic_write(&status_path(root), &text);
    }
}

/// One line in `distill.log`. The file is trimmed at 256KB, keeping the
/// tail — a log that grows forever is the F5 failure all over again.
fn log_line(root: &Path, msg: &str) {
    let path = root.join("distill.log");
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > LOG_MAX_BYTES
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let keep = &text[text.len().saturating_sub((LOG_MAX_BYTES / 2) as usize)..];
        let _ = crate::store::atomic_write(&path, keep);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", now_iso());
    }
}

/// `gray memory distill-log`: the last `n` lines of the distill log.
pub fn log_tail(home: &Path, n: usize) -> String {
    let path = home.join("memory").join("distill.log");
    match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(n);
            format!("{}\n", lines[start..].join("\n"))
        }
        _ => "No distill runs logged yet.\n".to_string(),
    }
}

/// One line for the bare `/memory` status: `last distill <ts> +N` when a
/// run completed, nothing otherwise.
pub fn last_status_line(home: &Path) -> Option<String> {
    let st = read_status(&home.join("memory"));
    if st.ts.is_empty() {
        return None;
    }
    Some(if st.last_failed {
        format!("last distill {} failed ({})", st.ts, st.note)
    } else {
        format!("last distill {} (+{})", st.ts, st.added)
    })
}

pub fn run(home: &Path, sid: &str, cwd: &Path, model: Option<&str>) {
    let root = home.join("memory");
    if crate::store::private_dir(&root).is_err() || root.join("distill.stopped").exists() {
        return;
    }
    let Ok(mut lockfile) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("distill.lock"))
    else {
        return;
    };
    if lockfile.try_lock().is_err() {
        // Another distill is in flight (§C.6): turns keep counting, the
        // next tick tries again.
        return;
    }
    let _ = lockfile.set_len(0);
    let _ = writeln!(
        lockfile,
        "pid={} session={} started={}",
        std::process::id(),
        sid,
        now_iso()
    );

    let outcome = distill_once(home, &root, sid, cwd, model);
    let mut st = read_status(&root);
    st.ts = now_iso();
    st.session = sid.to_string();
    match outcome {
        Ok(sum) => {
            st.consecutive_failures = 0;
            st.last_failed = false;
            st.added = sum.added;
            st.skipped = sum.skipped;
            st.errors = sum.errors;
            st.note = if sum.commands == 0 {
                "NOTHING".to_string()
            } else {
                format!("{} commands", sum.commands)
            };
            log_line(
                &root,
                &format!(
                    "distill {sid}: ok — {} added, {} skipped, {} errors{}",
                    sum.added,
                    sum.skipped,
                    sum.errors,
                    if sum.commands == 0 {
                        " (nothing to save)"
                    } else {
                        ""
                    }
                ),
            );
        }
        Err(e) => {
            st.consecutive_failures = (st.consecutive_failures + 1).min(99);
            st.last_failed = true;
            st.failed_ts = now_iso();
            st.note = format!("{e:#}");
            log_line(
                &root,
                &format!(
                    "distill {sid}: FAILED — {e:#} (consecutive {} of {MAX_FAILURES})",
                    st.consecutive_failures
                ),
            );
            if st.consecutive_failures >= MAX_FAILURES {
                let _ = std::fs::write(
                    root.join("distill.stopped"),
                    format!("{} {}\n", now_iso(), st.note),
                );
            }
        }
    }
    write_status(&root, &st);
}

struct Summary {
    commands: usize,
    added: usize,
    skipped: usize,
    errors: usize,
}

fn distill_once(
    home: &Path,
    root: &Path,
    sid: &str,
    cwd: &Path,
    model: Option<&str>,
) -> anyhow::Result<Summary> {
    ensure!(!crate::cli::disabled(), "memory disabled (GRAY_NO_MEMORY)");
    // The transcript can still be flushing when turn_end arrives — a `-p`
    // session is mid-exit — so retry for a few seconds before giving up.
    let deadline = Instant::now() + TRANSCRIPT_WAIT;
    let last = TRANSCRIPT_LAST.to_string();
    let transcript = loop {
        let mut read = Command::new("gray");
        read.args(["read", sid, "--last", &last]);
        match run_capped(&mut read, None, READ_TIMEOUT) {
            Ok(o) if o.status.is_some_and(|s| s.success()) && !o.stdout.trim().is_empty() => {
                break o.stdout;
            }
            Ok(o) if Instant::now() >= deadline => {
                anyhow::bail!("gray read failed: {}", o.stderr.trim())
            }
            Err(e) if Instant::now() >= deadline => return Err(e),
            _ => std::thread::sleep(Duration::from_secs(1)),
        }
    };
    let (project, user) = MemoryStore::new(home, cwd)
        .map(|s| {
            (
                s.list(Scope::Project).unwrap_or_default(),
                s.list(Scope::User).unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    let prompt = worker_prompt(sid, &project, &user, &transcript);

    // The worker is sandboxed two ways. It runs `--bare --lean` — no
    // plugins, no sidecars, no snapshot — and its GRAY_MEMORY_HOME is a
    // throwaway dir under the memory root, so even if it ignores the
    // prompt and runs `gray memory clear` itself it wipes a scratch
    // store, never the real one. GRAY_NO_MEMORY doubles that lock: every
    // memory verb in the worker's environment is disabled outright. The
    // ingest lines it emits are re-executed below against the real store
    // — and only the two ingest verbs are ever parsed.
    let scratch = root.join("worker-scratch");
    let _ = crate::store::private_dir(&scratch);
    let mut worker = Command::new("gray");
    worker
        .args(["--bare", "--lean", "-p", "-"])
        .env("GRAY_MEMORY_DISTILL", "1")
        .env("GRAY_MEMORY_HOME", &scratch)
        .env("GRAY_NO_MEMORY", "1")
        .current_dir(cwd);
    if let Some(m) = model {
        worker.args(["--model", m]);
    }
    let out =
        run_capped(&mut worker, Some(prompt), DISTILL_TIMEOUT).context("distill worker spawn")?;
    ensure!(
        !out.timed_out,
        "distill worker timed out ({DISTILL_TIMEOUT:?})"
    );
    ensure!(
        out.status.is_some_and(|s| s.success()),
        "distill worker exit {:?}: {}",
        out.status,
        out.stderr.trim()
    );

    let cmds = parse_worker_commands(&out.stdout);
    let exe = std::env::current_exe()?;
    let mut sum = Summary {
        commands: cmds.len(),
        added: 0,
        skipped: 0,
        errors: 0,
    };
    for c in cmds.iter().take(MAX_COMMANDS) {
        let mut ingest = Command::new(&exe);
        ingest
            .arg("--scope")
            .arg(if c.user_scope { "user" } else { "project" })
            .arg(if c.edit { "ingest-edit" } else { "ingest-set" })
            .arg(&c.key)
            .arg(&c.text)
            .current_dir(cwd)
            .env("GRAY_SESSION_ID", sid);
        match run_capped(&mut ingest, None, INGEST_TIMEOUT) {
            Ok(o) if o.status.is_some_and(|s| s.success()) => sum.added += 1,
            Ok(o) => {
                // Contract refusals (existing key, dup text, daily cap)
                // are skips, not failures — the store did its job.
                let msg = format!("{}{}", o.stdout, o.stderr);
                if msg.contains("exists")
                    || msg.contains("already stored")
                    || msg.contains("cap reached")
                    || msg.contains("unchanged")
                {
                    sum.skipped += 1;
                } else {
                    sum.errors += 1;
                    log_line(
                        root,
                        &format!(
                            "distill {sid}: ingest refused '{}' — {}",
                            c.key,
                            msg.trim().lines().next().unwrap_or("")
                        ),
                    );
                }
            }
            Err(e) => {
                sum.errors += 1;
                log_line(
                    root,
                    &format!("distill {sid}: ingest '{}' failed: {e:#}", c.key),
                );
            }
        }
    }
    if cmds.len() > MAX_COMMANDS {
        log_line(
            root,
            &format!(
                "distill {sid}: dropped {} excess emitted commands",
                cmds.len() - MAX_COMMANDS
            ),
        );
    }
    Ok(sum)
}

/// The worker sees the transcript tail and the store — everything it
/// needs to say NOTHING or emit a small number of ingest lines.
fn worker_prompt(sid: &str, project: &str, user: &str, transcript: &str) -> String {
    // Keep the freshest tail when the transcript is oversized.
    let tail = if transcript.len() > PROMPT_MAX_BYTES {
        let mut start = transcript.len() - PROMPT_MAX_BYTES;
        while !transcript.is_char_boundary(start) {
            start += 1;
        }
        &transcript[start..]
    } else {
        transcript
    };
    format!(
        "You are the memory distiller for the gray agent harness (session {sid}). \
Decide what from this transcript tail is worth remembering across future sessions.\n\
\n\
SAVE only durable facts: user preferences, confirmed project decisions, corrections \
of earlier decisions, stable environment or workflow facts. Do NOT save: transient \
task progress, TODOs, plans, file contents, anything already in memory, or secrets, \
credentials or tokens (they are refused anyway).\n\
\n\
MEMORY — project scope (this repo):\n\
{}\n\
MEMORY — user scope (cross-project preferences):\n\
{}\n\
TRANSCRIPT TAIL:\n\
{}\n\
\n\
Reply with one command per line and nothing else:\n\
  gray memory ingest-set KEY TEXT            new project-scope entry\n\
  gray memory ingest-set --scope user KEY TEXT\n\
  gray memory ingest-edit KEY TEXT           extend an existing entry — TEXT must keep the old text verbatim\n\
KEY is a short ASCII slug (letters, digits, '-' or '_'). TEXT is a single line that must \
contain `Why: <what prompted this>` and `falsified: <what would disprove it, or \
\"nothing yet\">`.\n\
Never emit `set`, `edit`, `remove` or `clear` — they do not exist here.\n\
If nothing is worth saving, reply exactly: NOTHING",
        if project.trim().is_empty() {
            "(empty)"
        } else {
            project.trim_end()
        },
        if user.trim().is_empty() {
            "(empty)"
        } else {
            user.trim_end()
        },
        tail
    )
}

// ---------------------------------------------------------------------------
// Worker output → ingest commands (the structural sandbox)
// ---------------------------------------------------------------------------

/// One parsed `gray memory ingest-set|ingest-edit` line.
#[derive(Debug, PartialEq)]
pub(crate) struct Parsed {
    pub edit: bool,
    pub user_scope: bool,
    pub key: String,
    pub text: String,
}

/// Scan worker output for `gray memory ingest-*` lines. Everything else —
/// prose, fences, `set`/`edit`/`remove`/`clear`, other CLIs — is ignored,
/// which is what makes the worker's write surface ingest-only.
pub(crate) fn parse_worker_commands(out: &str) -> Vec<Parsed> {
    let mut v = Vec::new();
    for raw in out.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['-', '*', '•', '›', '>'])
            .trim();
        let mut toks = line.split_whitespace();
        if toks.next() != Some("gray") || toks.next() != Some("memory") {
            continue;
        }
        let mut user_scope = false;
        let mut positional = Vec::new();
        let mut rest = toks.peekable();
        while let Some(t) = rest.next() {
            match t {
                "--scope" => {
                    user_scope = rest.next() == Some("user");
                }
                s if s.starts_with("--") => {}
                _ => positional.push(t),
            }
        }
        if positional.len() < 3 {
            continue;
        }
        let edit = match positional[0] {
            "ingest-set" => false,
            "ingest-edit" => true,
            _ => continue,
        };
        let key = positional[1].to_string();
        let mut text = positional[2..].join(" ");
        // Strip one layer of matching outer quotes the model may add.
        if text.len() >= 2 {
            let b = text.as_bytes();
            if (b[0] == b'"' && b[text.len() - 1] == b'"')
                || (b[0] == b'\'' && b[text.len() - 1] == b'\'')
            {
                text = text[1..text.len() - 1].to_string();
            }
        }
        if text.is_empty() {
            continue;
        }
        v.push(Parsed {
            edit,
            user_scope,
            key,
            text,
        });
    }
    v
}

// ---------------------------------------------------------------------------
// `run_capped`: spawn, drain both pipes, wait with a deadline
// ---------------------------------------------------------------------------

struct Captured {
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// `Command::output()` with a hard timeout: both pipes drain on threads
/// (a full pipe can't deadlock the wait), stdin gets `stdin_data`, and a
/// child past the deadline is killed and reaped.
fn run_capped(
    cmd: &mut Command,
    stdin_data: Option<String>,
    timeout: Duration,
) -> anyhow::Result<Captured> {
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn")?;
    let stdin_h = child.stdin.take();
    let mut stdout_h = child.stdout.take();
    let mut stderr_h = child.stderr.take();
    let writer = stdin_h.zip(stdin_data).map(|(mut s, data)| {
        std::thread::spawn(move || {
            let _ = s.write_all(data.as_bytes());
        })
    });
    let reader_out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = stdout_h.take() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let reader_err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = stderr_h.take() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break None,
        }
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    let stdout = reader_out.join().unwrap_or_default();
    let stderr = reader_err.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ingest_set_with_quoted_text() {
        let out = "gray memory ingest-set release-branch \"release/rhubarb. Why: user stated it; falsified: nothing yet\"";
        let cmds = parse_worker_commands(out);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].key, "release-branch");
        assert!(!cmds[0].edit);
        assert!(!cmds[0].user_scope);
        assert_eq!(
            cmds[0].text,
            "release/rhubarb. Why: user stated it; falsified: nothing yet"
        );
    }

    #[test]
    fn scope_and_edit_parse() {
        let out = "gray memory ingest-edit --scope user editor-pref prefers vim Why: correction falsified: nothing yet";
        let cmds = parse_worker_commands(out);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].edit);
        assert!(cmds[0].user_scope);
        assert_eq!(cmds[0].key, "editor-pref");
        assert_eq!(
            cmds[0].text,
            "prefers vim Why: correction falsified: nothing yet"
        );
    }

    #[test]
    fn wipe_verbs_and_prose_are_ignored() {
        let out = "I will now update memory:\n```\ngray memory set x wipe\ngray memory clear\ngray memory remove old\ngray memory ingest-set k \"t Why: w falsified: n\"\n```\nDone.";
        let cmds = parse_worker_commands(out);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].key, "k");
    }

    #[test]
    fn nothing_yields_no_commands() {
        assert!(parse_worker_commands("NOTHING").is_empty());
        assert!(parse_worker_commands("  \nNothing to save.\n").is_empty());
    }

    #[test]
    fn bullet_prefixed_lines_still_parse() {
        let out =
            "- gray memory ingest-set deploy-freeze Fridays Why: stated falsified: nothing yet";
        let cmds = parse_worker_commands(out);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].key, "deploy-freeze");
    }

    #[test]
    fn missing_text_is_dropped() {
        assert!(parse_worker_commands("gray memory ingest-set lonely-key").is_empty());
    }
}
