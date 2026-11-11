//! SQLite: the source of truth for files, sessions, turns, entities, vectors,
//! LLM spend, the rewrite cache and eval feedback. Search indexes are derived
//! from it and can be rebuilt.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS files (
    src_path TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    archive_path TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    prefix_hash TEXT NOT NULL,
    session_id TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    parent_id TEXT,
    host TEXT NOT NULL,
    cwd TEXT,
    title TEXT,
    title_is_prompt INTEGER NOT NULL DEFAULT 0,
    recap TEXT,
    model TEXT,
    started INTEGER,
    ended INTEGER,
    turn_count INTEGER NOT NULL DEFAULT 0,
    archive_path TEXT NOT NULL,
    bytes INTEGER NOT NULL DEFAULT 0,
    repo_key TEXT,
    repo_origin TEXT,
    skip INTEGER NOT NULL DEFAULT 0,
    bad_lines INTEGER NOT NULL DEFAULT 0,
    summary TEXT,
    llm_title TEXT,
    kind TEXT,
    outcome TEXT,
    tags TEXT,
    summary_turns INTEGER,
    card_text TEXT,
    card_hash TEXT,
    dirty INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    ts INTEGER,
    branch TEXT,
    cwd TEXT,
    user_text TEXT NOT NULL,
    assistant_text TEXT NOT NULL,
    tool_text TEXT NOT NULL,
    files TEXT NOT NULL,
    commands TEXT NOT NULL,
    errors TEXT NOT NULL,
    tools_json TEXT NOT NULL,
    ukey TEXT NOT NULL,
    chash TEXT NOT NULL,
    -- Written by the index pass: the prompt without boilerplate paragraphs,
    -- whether an earlier session owns this turn, and the embedding key.
    clean_user TEXT,
    dup INTEGER NOT NULL DEFAULT 0,
    vhash TEXT
);
CREATE INDEX IF NOT EXISTS turns_session ON turns(session_id, idx);
CREATE INDEX IF NOT EXISTS turns_ukey ON turns(ukey);

CREATE TABLE IF NOT EXISTS entities (
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    display TEXT NOT NULL,
    origin TEXT NOT NULL,
    strength INTEGER NOT NULL,
    PRIMARY KEY (session_id, kind, key)
);
CREATE INDEX IF NOT EXISTS entities_key ON entities(kind, key);

CREATE TABLE IF NOT EXISTS blocks (
    hash TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    PRIMARY KEY (hash, session_id)
);

CREATE TABLE IF NOT EXISTS cwd_repo (
    cwd TEXT PRIMARY KEY,
    repo_key TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS vecs (
    hash TEXT PRIMARY KEY,
    v BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS llm_usage (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    purpose TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    usd REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS rewrite_cache (
    q TEXT PRIMARY KEY,
    json TEXT NOT NULL,
    ts INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS feedback (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    query TEXT NOT NULL,
    session_id TEXT NOT NULL,
    label TEXT NOT NULL
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub id: String,
    pub source: String,
    pub parent_id: Option<String>,
    pub host: String,
    pub cwd: Option<String>,
    pub title: String,
    pub recap: Option<String>,
    pub summary: Option<String>,
    pub kind: Option<String>,
    pub outcome: Option<String>,
    pub tags: Vec<String>,
    pub model: Option<String>,
    pub started: i64,
    pub ended: i64,
    pub turn_count: i64,
    pub repo_key: Option<String>,
    pub repo_origin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Entity {
    pub kind: String,
    pub key: String,
    pub display: String,
    pub origin: String,
    pub strength: i64,
}

/// Session metadata and entities, held in memory for filtering, facets and
/// rendering. Rebuilt after every index pass.
#[derive(Default)]
pub struct Catalog {
    pub sessions: HashMap<String, SessionMeta>,
    pub entities: HashMap<String, Vec<Entity>>,
    /// (kind, lowercased key) → sessions.
    pub by_entity: HashMap<(String, String), Vec<String>>,
    /// Repo key → display name.
    pub repos: HashMap<String, String>,
    pub children: HashMap<String, Vec<String>>,
}

impl Catalog {
    pub fn load(c: &Connection) -> Result<Self> {
        let mut cat = Catalog::default();
        let mut st = c.prepare(
            "SELECT id, source, parent_id, host, cwd,
                    COALESCE(llm_title, title, ''), recap, summary, kind, outcome, tags, model,
                    COALESCE(started, 0), COALESCE(ended, 0), turn_count, repo_key, repo_origin
             FROM sessions WHERE skip = 0 AND turn_count > 0",
        )?;
        let rows = st.query_map([], |r| {
            Ok(SessionMeta {
                id: r.get(0)?,
                source: r.get(1)?,
                parent_id: r.get(2)?,
                host: r.get(3)?,
                cwd: r.get(4)?,
                title: r.get(5)?,
                recap: r.get(6)?,
                summary: r.get(7)?,
                kind: r.get(8)?,
                outcome: r.get(9)?,
                tags: r
                    .get::<_, Option<String>>(10)?
                    .map(|t| {
                        t.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|x| !x.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                model: r.get(11)?,
                started: r.get(12)?,
                ended: r.get(13)?,
                turn_count: r.get(14)?,
                repo_key: r.get(15)?,
                repo_origin: r.get(16)?,
            })
        })?;
        for s in rows {
            let s = s?;
            if let Some(p) = &s.parent_id {
                cat.children
                    .entry(p.clone())
                    .or_default()
                    .push(s.id.clone());
            }
            cat.sessions.insert(s.id.clone(), s);
        }
        let mut st = c.prepare("SELECT session_id, kind, key, display, origin, strength FROM entities ORDER BY strength DESC")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Entity {
                    kind: r.get(1)?,
                    key: r.get(2)?,
                    display: r.get(3)?,
                    origin: r.get(4)?,
                    strength: r.get(5)?,
                },
            ))
        })?;
        for row in rows {
            let (sid, e) = row?;
            if !cat.sessions.contains_key(&sid) {
                continue;
            }
            cat.by_entity
                .entry((e.kind.clone(), e.key.to_lowercase()))
                .or_default()
                .push(sid.clone());
            cat.entities.entry(sid).or_default().push(e);
        }
        for s in cat.sessions.values() {
            if let Some(k) = &s.repo_key {
                cat.repos.insert(k.clone(), crate::extract::repo_display(k));
            }
        }
        Ok(cat)
    }

    pub fn repo_name(&self, s: &SessionMeta) -> Option<String> {
        s.repo_key.as_ref().map(|k| {
            self.repos
                .get(k)
                .cloned()
                .unwrap_or_else(|| crate::extract::repo_display(k))
        })
    }

    pub fn entities_of(&self, id: &str, kind: &str) -> Vec<&Entity> {
        self.entities
            .get(id)
            .map(|v| v.iter().filter(|e| e.kind == kind).collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub idx: i64,
    pub ts: Option<i64>,
    pub branch: Option<String>,
    pub user_text: String,
    pub assistant_text: String,
    pub tools_json: String,
}

pub fn turns(c: &Connection, session: &str) -> Result<Vec<TurnRow>> {
    let mut st = c.prepare("SELECT idx, ts, branch, user_text, assistant_text, tools_json FROM turns WHERE session_id = ? ORDER BY idx")?;
    let rows = st.query_map([session], |r| {
        Ok(TurnRow {
            idx: r.get(0)?,
            ts: r.get(1)?,
            branch: r.get(2)?,
            user_text: r.get(3)?,
            assistant_text: r.get(4)?,
            tools_json: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn turn_texts(c: &Connection, ids: &[i64]) -> Result<HashMap<i64, (i64, String, String)>> {
    let mut out = HashMap::new();
    let mut st =
        c.prepare_cached("SELECT idx, user_text, assistant_text FROM turns WHERE id = ?")?;
    for id in ids {
        if let Some(row) = st
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
        {
            out.insert(*id, row);
        }
    }
    Ok(out)
}

pub fn get_meta(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
            .optional()?,
    )
}

pub fn set_meta(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
        params![key, value],
    )?;
    Ok(())
}

pub fn month_spend(c: &Connection) -> Result<f64> {
    let start = crate::util::month_start();
    Ok(c.query_row(
        "SELECT COALESCE(SUM(usd), 0) FROM llm_usage WHERE ts >= ?",
        [start],
        |r| r.get(0),
    )?)
}

pub fn record_usage(
    c: &Connection,
    purpose: &str,
    model: &str,
    input: i64,
    output: i64,
    usd: f64,
) -> Result<()> {
    c.execute(
        "INSERT INTO llm_usage (ts, purpose, model, input_tokens, output_tokens, usd) VALUES (?, ?, ?, ?, ?, ?)",
        params![chrono::Utc::now().timestamp(), purpose, model, input, output, usd],
    )?;
    Ok(())
}
