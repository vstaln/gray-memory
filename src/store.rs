//! Bounded, curated Markdown memory. One memory home is one trusted owner.
//! Commands use the existing bash surface; no extraction service or new tool.
//!
//! Ported verbatim from gray `crates/gray/src/memory.rs` (the feature moved
//! out of core into this sidecar plugin); `crate::` helpers live in lib.rs.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use clap::ValueEnum;

use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Scope {
    User,
    #[default]
    Project,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    pub ts: String,
    pub scope: String,
    pub key: String,
    pub old_text: String,
    pub op: String,
    pub session: String,
}

/// First sentence of an entry, without its " Why:" trailer: cut at the
/// first ". " whose preceding token is at least 2 characters or absent, so
/// "e.g. ", "i.e. " and "3.5 " never end a sentence but "(54%). " does.
/// The result is capped at [`SUMMARY_CHARS`], ending in "…" when cut.
fn first_sentence(text: &str) -> String {
    let head = match text.find(" Why:") {
        Some(i) => text[..i].trim_end(),
        None => text,
    };
    let cut = sentence_end(head).unwrap_or(head);
    if cut.chars().count() <= SUMMARY_CHARS {
        return cut.to_string();
    }
    let mut s: String = cut.chars().take(SUMMARY_CHARS - 1).collect();
    s.push('…');
    s
}

fn sentence_end(text: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(rel) = text[from..].find(". ") {
        let dot = from + rel;
        // The token is the alphanumeric run touching the period, so an
        // abbreviation's own dot ("e.g.", "3.5") is not part of it.
        let mut start = dot;
        while start > 0 {
            let Some(prev) = text[..start].chars().next_back() else {
                break;
            };
            if !prev.is_alphanumeric() {
                break;
            }
            start -= prev.len_utf8();
        }
        if start == dot || dot - start >= 2 {
            return Some(&text[..dot + 1]);
        }
        from = dot + 2;
    }
    None
}

pub struct MemoryStore {
    root: PathBuf,
    project: String,
    terms: Vec<String>,
}

impl MemoryStore {
    pub fn new(home: &Path, cwd: &Path) -> anyhow::Result<Self> {
        let cwd = cwd
            .canonicalize()
            .context("cannot resolve memory project directory")?;
        let project_root = crate::find_git_root(&cwd).unwrap_or(cwd);
        let project = format!(
            "{:x}",
            Sha256::digest(project_root.as_os_str().as_encoded_bytes())
        );
        let terms = project_terms(&project_root);
        Ok(Self {
            root: home.join("memory"),
            project,
            terms,
        })
    }

    fn path(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.root.join("user.md"),
            Scope::Project => self.root.join(format!("project-{}.md", self.project)),
        }
    }

    /// Raw store from disk, provenance included. The write path needs the
    /// trailers; the served path must never see them.
    fn read_store(&self, scope: Scope) -> anyhow::Result<Store> {
        let text = read_text(&self.path(scope))?.unwrap_or_default();
        parse(&text)
    }

    /// Total curated entries across both scopes; an unreadable or missing
    /// store reads as empty (a missing store *is* an empty store).
    pub fn entry_count(&self) -> usize {
        [Scope::User, Scope::Project]
            .into_iter()
            .map(|s| {
                self.read_store(s)
                    .map(|store| store.entries.len())
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Every entry in the scope as (key, text), sorted by key. A missing
    /// store reads as empty (a missing store *is* an empty store); a store
    /// that exists but cannot be read or parsed returns the error, leaving
    /// the caller to say so rather than reporting an empty scope.
    /// Text is returned whole: callers decide how much to show.
    pub fn entries(&self, scope: Scope) -> anyhow::Result<Vec<(String, String)>> {
        Ok(self.read_store(scope)?.entries.into_iter().collect())
    }

    pub fn snapshot(&self, session: Option<&str>) -> anyhow::Result<String> {
        Ok(match self.serve(session, None)? {
            Served::Full(text) | Served::Delta(text) => text,
        })
    }

    pub fn serve(
        &self,
        session: Option<&str>,
        transcript: Option<&Path>,
    ) -> anyhow::Result<Served> {
        let index = self.current_index()?;
        let Some(id) = session else {
            return Ok(Served::Full(index.render()));
        };
        let id = match crate::canonical_uuid(id) {
            Some(u) => u,
            None => {
                ensure!(crate::valid_session_id(id), "invalid memory session id");
                id.to_string()
            }
        };
        let dir = self.root.join("snapshots");
        private_dir(&dir)?;
        let path = dir.join(format!("{id}.json"));
        let _lock = lock(&dir.join(format!("{id}.lock")))?;
        let saved: Option<SessionState> = match read_text(&path)? {
            Some(text) => {
                let value: serde_json::Value =
                    serde_json::from_str(&text).context("invalid memory snapshot")?;
                ensure!(
                    value["project"].as_str() == Some(&self.project),
                    "memory snapshot belongs to another project"
                );
                serde_json::from_value(value)
                    .ok()
                    .filter(|s: &SessionState| s.v == 2)
            }
            None => None,
        };
        let fresh = saved.is_none();
        let mut state = saved.unwrap_or_default();
        let (boundaries, offset) = transcript
            .map(|t| count_compactions(t, state.offset, state.boundaries, fresh))
            .unwrap_or((state.boundaries, state.offset));
        let compacted = !fresh && boundaries > state.boundaries;
        state.boundaries = boundaries;
        state.offset = offset;
        let mut current = index.served_map();
        if !(fresh || compacted) {
            for (k, h) in &state.served {
                if !current.contains_key(k) && index.alive.contains(k) {
                    current.insert(k.clone(), h.clone());
                }
            }
        }
        let out = if fresh || compacted {
            state.v = 2;
            state.project = self.project.clone();
            state.base = index.render();
            state.served = current;
            Served::Full(state.base.clone())
        } else if state.served == current {
            Served::Full(state.base.clone())
        } else {
            state.seq += 1;
            let delta = render_delta(state.seq, &state.served, &current);
            state.served = current;
            Served::Delta(delta)
        };
        atomic_write(&path, &serde_json::to_string(&state)?)?;
        if fresh {
            prune_snapshots(&dir);
        }
        Ok(out)
    }

    fn current_index(&self) -> anyhow::Result<Index> {
        let (project, project_dropped) = self.index(Scope::Project)?;
        let (user, user_dropped) = self.index(Scope::User)?;
        let meta = self.read_meta();
        let mut alive = std::collections::BTreeSet::new();
        for scope in [Scope::Project, Scope::User] {
            for key in self.read_store(scope)?.entries.into_keys() {
                if !meta.stale.contains_key(&self.meta_key(scope, &key)) {
                    alive.insert(format!("{}/{key}", scope_label(scope)));
                }
            }
        }
        Ok(Index {
            project,
            project_dropped,
            user,
            user_dropped,
            alive,
        })
    }

    fn index(&self, scope: Scope) -> anyhow::Result<(Vec<(String, String)>, usize)> {
        let store = self.read_store(scope)?;
        let meta = self.read_meta();
        let mut rows: Vec<(usize, &str, &String, &String)> = store
            .entries
            .iter()
            .filter(|(k, _)| !meta.stale.contains_key(&self.meta_key(scope, k)))
            .map(|(k, v)| {
                let hay = format!("{k} {v}").to_lowercase();
                let score = self
                    .terms
                    .iter()
                    .filter(|t| hay.contains(t.as_str()))
                    .count();
                (score, saved_on(&store, k), k, v)
            })
            .collect();
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(a.1))
                .then_with(|| a.2.cmp(b.2))
        });
        let mut kept = Vec::new();
        let mut used = 0;
        let mut dropped = 0;
        for (_, _, key, text) in rows {
            let hint = hint(key, text);
            let line = key.len() + hint.len() + 5;
            if used + line > SNAPSHOT_SCOPE_BYTES - CAP_NOTICE_RESERVE {
                dropped += 1;
                continue;
            }
            used += line;
            kept.push((key.clone(), hint));
        }
        Ok((kept, dropped))
    }

    pub fn list(&self, scope: Scope) -> anyhow::Result<String> {
        Ok(render_served(&self.read_store(scope)?.entries))
    }

    pub fn list_filtered(&self, scope: Scope, all: bool) -> anyhow::Result<String> {
        let store = self.read_store(scope)?;
        let meta = self.read_meta();
        Ok(store
            .entries
            .iter()
            .filter_map(|(k, v)| match meta.stale.get(&self.meta_key(scope, k)) {
                Some(reason) if all => Some(format!("- {k}: {v} [stale: {reason}]\n")),
                Some(_) => None,
                None => Some(format!("- {k}: {v}\n")),
            })
            .collect())
    }

    pub fn profile(&self, scope: Scope) -> anyhow::Result<String> {
        let (rows, dropped) = self.index(scope)?;
        Ok(render_rows(&rows, dropped))
    }

    pub fn show(&self, scope: Scope, key: &str) -> anyhow::Result<Option<(Scope, String)>> {
        validate_key(key)?;
        let Some(found) = self.scope_holding(scope, key)? else {
            return Ok(None);
        };
        let text = self.get(found, key)?.unwrap_or_default();
        let id = self.meta_key(found, key);
        let _ = self.update_meta(|m| {
            m.used.insert(id, today());
        });
        Ok(Some((found, text)))
    }

    pub fn similar(&self, key: &str, text: &str) -> Vec<(Scope, String, String)> {
        let new_keys = key_tokens(key);
        let mut new_words = content_words(decision_of(text));
        new_words.extend(new_keys.iter().cloned());
        let mut hits: Vec<(bool, usize, f64, Scope, String, String)> = Vec::new();
        for scope in [Scope::Project, Scope::User] {
            let Ok(store) = self.read_store(scope) else {
                continue;
            };
            for (k, v) in &store.entries {
                let exact = k == key;
                let ks = key_tokens(k);
                let shared_keys = new_keys.intersection(&ks).count();
                let mut words = content_words(decision_of(v));
                words.extend(ks);
                let shared = new_words.intersection(&words).count();
                let union = new_words.union(&words).count().max(1);
                let jaccard = shared as f64 / union as f64;
                if exact || shared_keys >= 2 || (shared >= 2 && jaccard >= 0.3) {
                    hits.push((exact, shared_keys, jaccard, scope, k.clone(), hint(k, v)));
                }
            }
        }
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| b.2.total_cmp(&a.2))
        });
        hits.into_iter()
            .take(3)
            .map(|(_, _, _, s, k, h)| (s, k, h))
            .collect()
    }

    pub fn review(&self, mark: bool, keep: Option<&str>) -> anyhow::Result<String> {
        if let Some(key) = keep {
            validate_key(key)?;
            let ids: Vec<String> = [Scope::Project, Scope::User]
                .into_iter()
                .map(|s| self.meta_key(s, key))
                .collect();
            let mut found = false;
            self.update_meta(|m| {
                for id in &ids {
                    if m.stale.remove(id).is_some() {
                        found = true;
                        m.used.insert(id.clone(), today());
                    }
                }
            })?;
            return Ok(if found {
                format!("'{key}' kept: back in the index.\n")
            } else {
                format!("'{key}' is not marked stale.\n")
            });
        }
        let meta = self.read_meta();
        let cutoff = (chrono::Local::now() - chrono::Duration::days(UNUSED_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let mut found: Vec<(String, String, String)> = Vec::new();
        let mut already = 0;
        for scope in [Scope::Project, Scope::User] {
            let store = self.read_store(scope)?;
            for (key, text) in &store.entries {
                let id = self.meta_key(scope, key);
                if meta.stale.contains_key(&id) {
                    already += 1;
                    continue;
                }
                let mut reasons = Vec::new();
                for p in mentioned_paths(text) {
                    if !expand_home(&p).exists() {
                        reasons.push(format!("path missing: {p}"));
                    }
                }
                let saved = saved_on(&store, key);
                let used = meta.used.get(&id).map(String::as_str).unwrap_or("");
                let last = saved.max(used);
                if last < cutoff.as_str() {
                    reasons.push(if last.is_empty() {
                        "no recorded use".to_string()
                    } else {
                        format!("unused since {last}")
                    });
                }
                if !reasons.is_empty() {
                    found.push((
                        id,
                        format!("{}/{key}", scope_label(scope)),
                        reasons.join("; "),
                    ));
                }
            }
        }
        let mut out = String::new();
        if found.is_empty() {
            out.push_str("No new stale entries.\n");
        } else {
            for (_, label, reason) in &found {
                out.push_str(&format!("- {label}: {reason}\n"));
            }
            if mark {
                let stamp = today();
                self.update_meta(|m| {
                    for (id, _, reason) in &found {
                        m.stale.insert(id.clone(), format!("{stamp} {reason}"));
                    }
                })?;
                out.push_str(&format!(
                    "Marked {} stale: out of the index, still in `gray memory list --all`. Nothing deleted; `gray memory review --keep KEY` brings one back, `gray memory remove KEY` deletes it.\n",
                    found.len()
                ));
            } else {
                out.push_str("Dry run: nothing marked.\n");
            }
        }
        if already > 0 {
            out.push_str(&format!("{already} entries were already stale.\n"));
        }
        Ok(out)
    }

    fn scope_id(&self, scope: Scope) -> String {
        match scope {
            Scope::User => "user".to_string(),
            Scope::Project => format!("project-{}", self.project),
        }
    }

    fn meta_key(&self, scope: Scope, key: &str) -> String {
        format!("{}:{key}", self.scope_id(scope))
    }

    fn read_meta(&self) -> Meta {
        read_text(&self.root.join("meta.json"))
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn update_meta(&self, f: impl FnOnce(&mut Meta)) -> anyhow::Result<()> {
        private_dir(&self.root)?;
        let _lock = lock(&self.root.join("meta.lock"))?;
        let mut meta = self.read_meta();
        f(&mut meta);
        atomic_write(&self.root.join("meta.json"), &serde_json::to_string(&meta)?)
    }

    /// CLI view with provenance, so a human can see how old each entry is and
    /// which session wrote it. Separate from [`list`](Self::list) so the
    /// served format stays byte-identical.
    pub fn list_detailed(&self, scope: Scope) -> anyhow::Result<String> {
        let store = self.read_store(scope)?;
        Ok(store
            .entries
            .iter()
            .map(|(key, text)| {
                let provenance = store.provenance.get(key);
                let saved = provenance
                    .and_then(|p| p.saved.as_deref())
                    .unwrap_or("unknown date");
                let source = provenance
                    .and_then(|p| p.source.as_deref())
                    .unwrap_or("unknown source");
                format!("- {key}: {text}\n    saved {saved} by {source}\n")
            })
            .collect())
    }

    pub fn set(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_key(key)?;
        validate_text(text)?;
        self.change(scope, |entries| {
            if entries.get(key).is_some_and(|old| old == text.trim())
                || (!entries.contains_key(key) && entries.values().any(|v| v == text.trim()))
            {
                return Ok(false);
            }
            entries.insert(key.to_owned(), text.trim().to_owned());
            Ok(true)
        })
    }

    /// One entry's text, if the scope holds it.
    pub fn get(&self, scope: Scope, key: &str) -> anyhow::Result<Option<String>> {
        validate_key(key)?;
        Ok(self.read_store(scope)?.entries.remove(key))
    }

    /// Rewrite the text of an existing entry; never creates a new one.
    /// Reports whether the text actually changed.
    pub fn edit(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_key(key)?;
        validate_text(text)?;
        let trimmed = text.trim().to_owned();
        let changed = self.change(scope, |entries| {
            ensure!(entries.contains_key(key), "no memory entry named '{key}'");
            if entries.get(key).is_some_and(|old| *old == trimmed) {
                return Ok(false);
            }
            entries.insert(key.to_owned(), trimmed);
            Ok(true)
        })?;
        Ok(changed)
    }

    /// Daily-ingest add. Unlike [`set`](Self::set) this can never replace an
    /// existing entry, must carry the rationale contract, and spends from a
    /// hard daily budget: the prompt contract leaked once already (a dry run
    /// overwrote five entries and invented a quote), so the verbs enforce it.
    pub fn ingest_set(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_key(key)?;
        validate_text(text)?;
        ensure!(has_rationale(text), INGEST_RATIONALE_HINT);
        let trimmed = text.trim().to_owned();
        let changed = self.change(scope, |entries| {
            ensure!(
                !entries.contains_key(key),
                "memory entry '{key}' exists; the daily ingest never overwrites — skip it, or use ingest-edit to record a contradiction"
            );
            ensure!(
                !entries.values().any(|old| old == &trimmed),
                "that text is already stored under another key; the ingest never duplicates — skip it"
            );
            entries.insert(key.to_owned(), trimmed.clone());
            Ok(true)
        })?;
        Ok(changed)
    }

    /// Daily-ingest reconcile. Append-only by construction: when the new text
    /// does not already carry the old one, `(was: <old>)` is appended here, so
    /// a contradiction is recorded, never a silent replacement. A key that is
    /// not there yet is simply created.
    pub fn ingest_edit(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_key(key)?;
        validate_text(text)?;
        ensure!(has_rationale(text), INGEST_RATIONALE_HINT);
        let trimmed = text.trim().to_owned();
        let changed = self.change(scope, |entries| {
            let Some(old) = entries.get(key).cloned() else {
                entries.insert(key.to_owned(), trimmed.clone());
                return Ok(true);
            };
            if old == trimmed {
                return Ok(false);
            }
            let merged = if trimmed.contains(&old) {
                trimmed.clone()
            } else {
                format!("{trimmed} (was: {old})")
            };
            entries.insert(key.to_owned(), merged);
            Ok(true)
        })?;
        Ok(changed)
    }

    /// Remove every entry in the scope; returns how many were dropped.
    pub fn clear(&self, scope: Scope) -> anyhow::Result<usize> {
        let mut dropped = 0;
        self.change(scope, |entries| {
            dropped = entries.len();
            entries.clear();
            Ok(dropped > 0)
        })?;
        Ok(dropped)
    }

    pub fn remove(&self, scope: Scope, key: &str) -> anyhow::Result<()> {
        validate_key(key)?;
        self.change(scope, |entries| {
            ensure!(entries.remove(key).is_some(), "memory entry not found");
            Ok(true)
        })?;
        Ok(())
    }

    pub fn update(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_key(key)?;
        validate_text(text)?;
        let trimmed = text.trim().to_owned();
        let scope = self.scope_holding(scope, key)?.unwrap_or(scope);
        if let Some(old) = self.get(scope, key)? {
            if old == trimmed {
                return Ok(false);
            }
            self.append_history(scope, key, &old, "update")?;
        }
        self.upsert(scope, key, &trimmed)
    }

    fn scope_holding(&self, scope: Scope, key: &str) -> anyhow::Result<Option<Scope>> {
        let other = match scope {
            Scope::User => Scope::Project,
            Scope::Project => Scope::User,
        };
        for candidate in [scope, other] {
            if self.get(candidate, key)?.is_some() {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    fn upsert(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        self.change(scope, |entries| {
            if entries.get(key).is_some_and(|old| old == text) {
                return Ok(false);
            }
            entries.insert(key.to_owned(), text.to_owned());
            Ok(true)
        })
    }

    pub fn ingest_update(&self, scope: Scope, key: &str, text: &str) -> anyhow::Result<bool> {
        validate_text(text)?;
        ensure!(has_rationale(text), INGEST_RATIONALE_HINT);
        self.update(scope, key, text)
    }

    pub fn remove_logged(&self, scope: Scope, key: &str) -> anyhow::Result<()> {
        validate_key(key)?;
        let scope = match self.scope_holding(scope, key)? {
            Some(found) => found,
            None => anyhow::bail!("no memory entry named '{key}'"),
        };
        let old = self
            .get(scope, key)?
            .with_context(|| format!("no memory entry named '{key}'"))?;
        self.append_history(scope, key, &old, "remove")?;
        self.remove(scope, key)
    }

    fn history_path(&self) -> PathBuf {
        self.root.join("history.jsonl")
    }

    fn append_history(
        &self,
        scope: Scope,
        key: &str,
        old_text: &str,
        op: &str,
    ) -> anyhow::Result<()> {
        private_dir(&self.root)?;
        let path = self.history_path();
        reject_symlinks(&path)?;
        let _lock = lock(&path.with_extension("lock"))?;
        let mut opts = OpenOptions::new();
        opts.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        let line = serde_json::json!({
            "ts": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "scope": scope_label(scope),
            "key": key,
            "old_text": old_text,
            "op": op,
            "session": std::env::var("GRAY_SESSION_ID").unwrap_or_default(),
        });
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn history_tail(&self, limit: usize) -> anyhow::Result<Vec<HistoryEntry>> {
        let Some(text) = read_text(&self.history_path())? else {
            return Ok(Vec::new());
        };
        let entries: Vec<HistoryEntry> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
            .collect();
        let start = entries.len().saturating_sub(limit);
        Ok(entries[start..].to_vec())
    }

    pub fn restore(&self, key: &str) -> anyhow::Result<(Scope, String)> {
        validate_key(key)?;
        let entry = self
            .history_tail(usize::MAX)?
            .into_iter()
            .rev()
            .find(|entry| entry.key == key)
            .with_context(|| format!("no history entry for '{key}'"))?;
        let scope = match entry.scope.as_str() {
            "user" => Scope::User,
            "project" => Scope::Project,
            other => anyhow::bail!("history entry has unknown scope '{other}'"),
        };
        let text = entry.old_text;
        validate_text(&text)?;
        self.change(scope, |entries| {
            entries.insert(key.to_owned(), text.clone());
            Ok(true)
        })?;
        Ok((scope, text))
    }

    fn change(
        &self,
        scope: Scope,
        update: impl FnOnce(&mut BTreeMap<String, String>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<bool> {
        private_dir(&self.root)?;
        let path = self.path(scope);
        let _lock = lock(&path.with_extension("lock"))?;
        let mut store = self.read_store(scope)?;
        let before = store.entries.clone();
        let changed = update(&mut store.entries)?;
        if changed {
            // Stamp every new or rewritten entry; drop provenance for entries
            // that no longer exist. A hand-edited line that lost its trailer
            // stays unstamped rather than being guessed at.
            let saved = today();
            let source = source_from(std::env::var("GRAY_SESSION_ID").ok().as_deref());
            store
                .provenance
                .retain(|k, _| store.entries.contains_key(k));
            for (key, text) in &store.entries {
                if before.get(key) != Some(text) {
                    let entry = store.provenance.entry(key.clone()).or_default();
                    entry.saved = Some(saved.clone());
                    entry.source = Some(source.clone());
                }
            }
            atomic_write(&path, &render(&store))?;
            let touched: Vec<String> = before
                .iter()
                .filter(|(k, v)| store.entries.get(*k) != Some(*v))
                .map(|(k, _)| self.meta_key(scope, k))
                .collect();
            if self.read_meta().stale.keys().any(|id| touched.contains(id)) {
                let _ = self.update_meta(|m| m.stale.retain(|id, _| !touched.contains(id)));
            }
            self.record_growth(
                scope,
                store.entries.len(),
                store.entries.len() < before.len(),
            );
        }
        Ok(changed)
    }

    /// Advisory review against the keep/delete rule. Never mutates anything:
    /// writing a rationale is safe, acting on one to delete is not, so the
    /// decision stays with a human.
    pub fn audit(&self, scope: Scope) -> anyhow::Result<String> {
        let store = self.read_store(scope)?;
        let entries = &store.entries;
        let mut findings: Vec<String> = Vec::new();
        let mut by_target: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, text) in &store.entries {
            if !text.contains("Why:") {
                findings.push(format!(
                    "- {key}: no Why recorded — what failure or correction prompted it?"
                ));
            }
            if records_falsified_outcome(text) {
                findings.push(format!(
                    "- {key}: records a falsified outcome — its failure kept recurring anyway; delete it or fold it into its replacement"
                ));
            }
            by_target
                .entry(normalized_target(text))
                .or_default()
                .push(key.clone());
        }
        for keys in by_target.values() {
            if keys.len() > 1 {
                findings.push(format!(
                    "- {}: duplicate target — fold into one entry",
                    keys.join(", ")
                ));
            }
        }
        let growth = self.growth(scope);
        let mut out = format!(
            "Audit ({} scope): {} entries.\n\n",
            scope_label(scope),
            entries.len()
        );
        if findings.is_empty() {
            out.push_str(
                "Every entry carries a why, no duplicate targets, no falsified outcomes.\n",
            );
        } else {
            findings.sort();
            out.push_str(&findings.join("\n"));
            out.push('\n');
        }
        out.push_str(&format!(
            "\nRule: if an entry's failure has not recurred since the entry was added, it is probably preventing that failure — keep it. Delete only when the failure kept recurring anyway or the entry duplicates another's target, and carry the removed entry's falsified attempts into its replacement.\nGrowth: {} entries, peak {}, {} net-add saves, {} removals.\nThis audit deletes nothing; a human decides.\n",
            entries.len(),
            growth.as_ref().map(|g| g.peak).unwrap_or(entries.len()),
            growth.as_ref().map(|g| g.streak).unwrap_or(0),
            growth.as_ref().map(|g| g.removals).unwrap_or(0),
        ));
        Ok(out)
    }

    /// The ratchet warning: repeated net growth with no removal is what
    /// unbounded prompt growth looks like before anyone notices (the
    /// paper's Gate-0 signal). Returns `None` until the streak trips.
    pub fn growth_warning(&self, scope: Scope) -> Option<String> {
        let g = self.growth(scope)?;
        (g.streak >= GROWTH_STREAK_WARN).then(|| {
            format!(
                "memory has grown to {} entries over {} saves with {} removals — consider `gray memory audit`",
                g.peak, g.streak, g.removals
            )
        })
    }

    fn growth(&self, scope: Scope) -> Option<Growth> {
        let text = std::fs::read_to_string(self.growth_path()).ok()?;
        let all: BTreeMap<String, Growth> = serde_json::from_str(&text).ok()?;
        all.get(scope_label(scope)).cloned()
    }

    fn growth_path(&self) -> PathBuf {
        self.root.join("growth.json")
    }

    fn growth_lock_path(&self) -> PathBuf {
        self.root.join("growth.lock")
    }

    /// Fold one change into the growth record. A removal resets the streak;
    /// a shrink lowers the peak; only net growth extends it.
    fn record_growth(&self, scope: Scope, count: usize, removed: bool) {
        let path = self.growth_path();
        // One lock for the whole file. The per-scope entry locks do not cover
        // it, so a user-scope and a project-scope save racing each other would
        // each read the file, update their own key, and clobber the other.
        let Ok(_lock) = lock(&self.growth_lock_path()) else {
            return;
        };
        let mut all: BTreeMap<String, Growth> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let g = all.entry(scope_label(scope).to_string()).or_default();
        if removed {
            g.removals += 1;
            g.streak = 0;
        }
        match count.cmp(&g.peak) {
            std::cmp::Ordering::Greater => {
                g.peak = count;
                g.streak += 1;
            }
            std::cmp::Ordering::Less => {
                g.peak = count;
                g.streak = 0;
            }
            std::cmp::Ordering::Equal => {}
        }
        if let Ok(text) = serde_json::to_string(&all) {
            let _ = atomic_write(&path, &text);
        }
    }
}

pub enum Served {
    Full(String),
    Delta(String),
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct SessionState {
    v: u32,
    project: String,
    base: String,
    served: BTreeMap<String, String>,
    seq: u64,
    boundaries: u64,
    offset: u64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Meta {
    used: BTreeMap<String, String>,
    stale: BTreeMap<String, String>,
}

struct Index {
    project: Vec<(String, String)>,
    project_dropped: usize,
    user: Vec<(String, String)>,
    user_dropped: usize,
    alive: std::collections::BTreeSet<String>,
}

impl Index {
    fn render(&self) -> String {
        let mut out = String::new();
        for (label, rows, dropped) in [
            ("Project", &self.project, self.project_dropped),
            ("User", &self.user, self.user_dropped),
        ] {
            if rows.is_empty() && dropped == 0 {
                continue;
            }
            out.push_str(&format!("{label}:\n{}", render_rows(rows, dropped)));
        }
        out
    }

    fn served_map(&self) -> BTreeMap<String, String> {
        self.project
            .iter()
            .map(|(k, h)| (format!("project/{k}"), h.clone()))
            .chain(
                self.user
                    .iter()
                    .map(|(k, h)| (format!("user/{k}"), h.clone())),
            )
            .collect()
    }
}

fn render_rows(rows: &[(String, String)], dropped: usize) -> String {
    let mut out: String = rows.iter().map(|(k, h)| format!("- {k}: {h}\n")).collect();
    if dropped > 0 {
        out.push_str(&format!(
            "{CAP_NOTICE_PREFIX}{dropped} older entries not shown; `gray memory list` prints them all)\n"
        ));
    }
    out
}

fn render_delta(
    seq: u64,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> String {
    let name = |k: &str| {
        k.split_once('/')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| k.to_string())
    };
    let mut out = format!("Memory changes #{seq} (+ added, ~ edited, - removed):\n");
    for (k, h) in new {
        match old.get(k) {
            None => out.push_str(&format!("+ {}: {h}\n", name(k))),
            Some(was) if was != h => out.push_str(&format!("~ {}: {h}\n", name(k))),
            Some(_) => {}
        }
    }
    for k in old.keys().filter(|k| !new.contains_key(*k)) {
        out.push_str(&format!("- {}\n", name(k)));
    }
    out
}

fn count_compactions(path: &Path, offset: u64, seen: u64, fresh: bool) -> (u64, u64) {
    use std::io::{Seek, SeekFrom};
    let Ok(mut file) = File::open(path) else {
        return (seen, offset);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if fresh {
        return (0, len);
    }
    let (start, base) = if len < offset { (0, 0) } else { (offset, seen) };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (seen, offset);
    }
    let mut buf = Vec::new();
    if file.take(len - start).read_to_end(&mut buf).is_err() {
        return (seen, offset);
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
        return (base, start);
    };
    let needle = b"\"compaction_boundary\"";
    let found = buf[..end]
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count() as u64;
    (base + found, start + end as u64 + 1)
}

const HINT_WORDS: usize = 8;

const UNUSED_DAYS: i64 = 30;

const HINT_CHARS: usize = 64;

fn hint(key: &str, text: &str) -> String {
    let mut plain = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let mut body = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    for mark in [",", ":", ";", "."] {
        body = body.replace(&format!(" {mark}"), mark);
    }
    for prefix in [format!("{key}:"), "Decision:".to_string()] {
        if let Some(rest) = body.strip_prefix(&prefix) {
            body = rest.trim_start().to_string();
        }
    }
    let sentence = first_sentence(&body);
    let words: Vec<&str> = sentence.split_whitespace().collect();
    let mut out = String::new();
    let mut cut = false;
    for (i, w) in words.iter().enumerate() {
        if i == HINT_WORDS || out.chars().count() + w.chars().count() + 1 > HINT_CHARS {
            cut = true;
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    if out.is_empty() {
        out = sentence.chars().take(HINT_CHARS - 1).collect();
        cut = sentence.chars().count() >= HINT_CHARS;
    }
    if cut && !out.ends_with('…') {
        out.push('…');
    }
    out
}

const STOP_WORDS: &[&str] = &[
    "that",
    "this",
    "with",
    "from",
    "when",
    "what",
    "never",
    "always",
    "only",
    "into",
    "than",
    "then",
    "they",
    "them",
    "their",
    "have",
    "been",
    "were",
    "will",
    "just",
    "like",
    "also",
    "each",
    "every",
    "does",
    "should",
    "because",
    "about",
    "after",
    "before",
    "which",
    "there",
    "here",
    "user",
    "stated",
    "session",
    "nothing",
    "falsified",
    "must",
    "used",
    "uses",
    "over",
];

fn key_tokens(key: &str) -> std::collections::BTreeSet<String> {
    key.split(['-', '_'])
        .map(str::to_lowercase)
        .filter(|t| t.len() >= 3)
        .collect()
}

fn content_words(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4 && !STOP_WORDS.contains(w))
        .map(str::to_string)
        .collect()
}

fn decision_of(text: &str) -> &str {
    text.split_once(" Why:").map_or(text, |(d, _)| d)
}

fn mentioned_paths(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|t| {
            t.trim_start_matches(['(', '[', '{', '"', '\'', '`'])
                .trim_end_matches([')', ']', '}', '"', '\'', '`', ',', ';', ':', '.', '…'])
        })
        .filter(|t| {
            let rooted = t.starts_with("~/") || (t.starts_with('/') && t[1..].contains('/'));
            rooted
                && t.len() > 2
                && !t.contains("://")
                && !t.contains(['*', '<', '>', '{', '}', '$', '|'])
        })
        .map(str::to_string)
        .collect()
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(path),
    }
}

const GENERIC_TERMS: &[&str] = &[
    "home", "users", "content", "src", "code", "projects", "repos", "work", "tmp", "admin",
];

fn project_terms(root: &Path) -> Vec<String> {
    let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let mut terms: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3 && !GENERIC_TERMS.contains(t))
        .map(str::to_string)
        .collect();
    terms.dedup();
    terms
}

/// How many consecutive net-growth saves with no removal trip the warning.
const GROWTH_STREAK_WARN: usize = 3;

const INGEST_RATIONALE_HINT: &str = "daily-ingest entries must carry `Why:` (the user's quoted failure or correction) and `falsified:` (`nothing yet` if none)";

fn has_rationale(text: &str) -> bool {
    text.contains("Why:") && text.contains("falsified:")
}

/// Per-scope growth record: the peak entry count, how many consecutive
/// net-growth saves produced it, and how many removals have happened.
#[derive(Default, Clone, serde::Deserialize, serde::Serialize)]
struct Growth {
    peak: usize,
    streak: usize,
    removals: usize,
}

pub(crate) fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
    }
}

/// An entry whose `falsified` field records a real failed attempt. The
/// convention's compliant value is "falsified: nothing yet" — an entry that
/// has not been contradicted must not read as one that has.
fn records_falsified_outcome(text: &str) -> bool {
    let Some((_, value)) = text.split_once("falsified:") else {
        return false;
    };
    let value = value
        .trim_start()
        .trim_end_matches(['.', ';'])
        .trim()
        .to_ascii_lowercase();
    !matches!(
        value.as_str(),
        "" | "nothing" | "nothing yet" | "none" | "n/a"
    )
}

/// Two entries aiming at the same thing, ignoring case and spacing: the
/// mechanical stand-in for "duplicates another directive's target". Only the
/// decision counts — two entries with the same aim and different rationales
/// are still duplicates of each other.
fn normalized_target(text: &str) -> String {
    let decision = text.split_once("Why:").map_or(text, |(d, _)| d);
    decision
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_key(key: &str) -> anyhow::Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 64
            && key
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "memory key must be 1-64 ASCII letters, digits, underscores or hyphens"
    );
    Ok(())
}

fn validate_text(text: &str) -> anyhow::Result<()> {
    ensure!(!text.trim().is_empty(), "memory text must not be empty");
    ensure!(!text.chars().any(|c| c.is_control() || matches!(c, '\u{00ad}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')), "memory text must be one line without control or invisible formatting characters");
    let redacted = crate::redact::redact_for_disclosure(text);
    ensure!(
        !redacted
            .kinds()
            .iter()
            .any(|k| k == crate::redact::REDACTION_SECRET),
        "memory text contains a possible credential; not saved"
    );
    Ok(())
}

/// First bytes of the line [`MemoryStore::profile`] appends when the cap
/// dropped entries.
const CAP_NOTICE_PREFIX: &str = "- (+";

fn parse(text: &str) -> anyhow::Result<Store> {
    let mut store = Store::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (key, value) = line
            .strip_prefix("- ")
            .and_then(|l| l.split_once(": "))
            .context("invalid memory Markdown; expected '- key: text'")?;
        validate_key(key)?;
        let (body, provenance) = split_trailer(value);
        validate_text(body)?;
        ensure!(
            store
                .entries
                .insert(key.to_owned(), body.to_owned())
                .is_none(),
            "duplicate key in memory file"
        );
        if provenance != Provenance::default() {
            store.provenance.insert(key.to_owned(), provenance);
        }
    }
    Ok(store)
}

/// Parsed store: entry text plus where each entry came from. Provenance is
/// kept out of `entries` so every caller that only wants text keeps working
/// unchanged.
#[derive(Default)]
struct Store {
    entries: BTreeMap<String, String>,
    provenance: BTreeMap<String, Provenance>,
}

/// Where an entry came from and when. Rides on the Markdown line as an
/// HTML-comment trailer so the store stays hand-editable, and is stripped
/// before the snapshot reaches the model (arXiv 2607.14611: a planted payload
/// in a memory file attacks future sessions, so the entries you can trace are
/// the ones you can purge).
#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Provenance {
    /// `YYYY-MM-DD`, the day the current text was written.
    saved: Option<String>,
    /// Session id, or `cli` for a direct command-line save.
    source: Option<String>,
}

const TRAILER_OPEN: &str = "<!-- gray:";

/// Split a trailing `<!-- gray:saved=...;source=... -->` off a line's value.
/// Only the end of the line is considered, so entry text containing HTML
/// comments is untouched.
fn split_trailer(value: &str) -> (&str, Provenance) {
    let none = Provenance::default();
    let Some(body) = value.strip_suffix("-->") else {
        return (value.trim_end(), none);
    };
    let Some(start) = body.rfind(TRAILER_OPEN) else {
        return (value.trim_end(), none);
    };
    let mut provenance = Provenance::default();
    for part in body[start + TRAILER_OPEN.len()..].split(';') {
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k.trim() {
                "saved" => provenance.saved = Some(v.to_owned()),
                "source" => provenance.source = Some(v.to_owned()),
                _ => {}
            }
        }
    }
    (value[..start].trim_end(), provenance)
}

/// The trailer for one entry, or empty when nothing is known.
fn trailer_for(provenance: &Provenance) -> String {
    let mut parts = Vec::new();
    if let Some(saved) = &provenance.saved {
        parts.push(format!("saved={saved}"));
    }
    if let Some(source) = &provenance.source {
        parts.push(format!("source={source}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {TRAILER_OPEN}{} -->", parts.join(";"))
    }
}

/// Today, local date. Same `%Y-%m-%d` shape the rest of gray stamps with.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Which session is asking. The REPL exports the session id into the tool
/// environment, so a `gray memory set` subprocess can name its parent; a
/// direct command-line save has no session and says so. Pure so the parallel
/// test suite never has to mutate the process environment.
fn source_from(session: Option<&str>) -> String {
    session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| "cli".to_owned(), str::to_owned)
}

fn render(store: &Store) -> String {
    store
        .entries
        .iter()
        .map(|(k, v)| {
            let trailer = store.provenance.get(k).map(trailer_for).unwrap_or_default();
            format!("- {k}: {v}{trailer}\n")
        })
        .collect()
}

/// What the model sees: text only, never the trailers. Rationale costs the
/// editor tokens and not the executor's, and provenance is the same trade.
/// Byte budget for one scope's injected memory block: a snapshot that rides
/// in every turn's system prompt is billed every turn.
const SNAPSHOT_SCOPE_BYTES: usize = 1024;

const SUMMARY_CHARS: usize = 120;

const CAP_NOTICE_RESERVE: usize = 80;

/// The day an entry's current text was written, `""` when unrecorded.
fn saved_on<'a>(store: &'a Store, key: &str) -> &'a str {
    store
        .provenance
        .get(key)
        .and_then(|p| p.saved.as_deref())
        .unwrap_or("")
}

pub(crate) fn render_served(entries: &BTreeMap<String, String>) -> String {
    entries
        .iter()
        .map(|(k, v)| format!("- {k}: {v}\n"))
        .collect()
}

/// Refuse symlinks in managed paths, including ancestors; no path supplied by
/// memory text is ever opened. This is not a sandbox against the same OS user.
pub(crate) fn reject_symlinks(path: &Path) -> anyhow::Result<()> {
    for p in path.ancestors() {
        match std::fs::symlink_metadata(p) {
            Ok(m) => ensure!(
                !m.file_type().is_symlink(),
                "memory path contains a symbolic link"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("cannot inspect memory path"),
        }
    }
    Ok(())
}

/// Bound `snapshots/`: one `<session>.json` + `.lock` lands per durable
/// session and nothing ever removed them. Runs only on a fresh freeze —
/// replaying an existing snapshot never touches the directory. A pruned
/// session that is ever resumed simply re-freezes against current memory.
const SNAPSHOT_KEEP: usize = 512;

fn prune_snapshots(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .map(|p| {
            let m = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (m, p)
        })
        .collect();
    if files.len() <= SNAPSHOT_KEEP {
        return;
    }
    files.sort();
    let excess = files.len() - SNAPSHOT_KEEP;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn private_dir(path: &Path) -> anyhow::Result<()> {
    reject_symlinks(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Read a memory file whole. There is no size cap: the file belongs to the
/// same OS user and every entry was validated on the way in.
pub(crate) fn read_text(path: &Path) -> anyhow::Result<Option<String>> {
    reject_symlinks(path)?;
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("cannot read memory file"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "memory path is not a regular file"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(
        String::from_utf8(bytes).context("memory file is not UTF-8")?,
    ))
}

pub(crate) fn lock(path: &Path) -> anyhow::Result<File> {
    reject_symlinks(path)?;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "memory lock timeout; retry later"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("cannot lock memory"),
        }
    }
}

pub(crate) fn atomic_write(path: &Path, text: &str) -> anyhow::Result<()> {
    reject_symlinks(path)?;
    let parent = path.parent().context("memory path has no parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    // NamedTempFile is owner-only on Unix; persist replaces atomically on
    // supported platforms and removes the tempfile on error via RAII.
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .context("cannot persist memory")?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;
