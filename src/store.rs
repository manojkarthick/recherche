//! SQLite: the source of truth for files, sessions, turns, entities, vectors,
//! the rewrite cache and eval feedback. Search indexes are derived
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
    /// When the daemon first ran; sessions started earlier are only
    /// summarized by an explicit `recherche summarize`.
    pub auto_summary_since: i64,
}

impl Catalog {
    pub fn load(c: &Connection) -> Result<Self> {
        let mut cat = Catalog {
            auto_summary_since: get_meta(c, "auto_summary_since")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(i64::MAX),
            ..Default::default()
        };
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

    pub fn recent(
        &self,
        repo: Option<&str>,
        agent: Option<&str>,
        since: Option<i64>,
        limit: usize,
    ) -> (Vec<&SessionMeta>, usize) {
        let resolved_repo =
            repo.map(|r| crate::query::resolve_repo(self, r).unwrap_or_else(|| r.to_string()));
        let agent = agent.map(str::to_lowercase);
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .filter(|s| since.is_none_or(|since| s.ended >= since))
            .filter(|s| {
                resolved_repo.as_deref().is_none_or(|r| {
                    s.repo_key.as_deref() == Some(r)
                        || self.repo_name(s).is_some_and(|n| n.eq_ignore_ascii_case(r))
                })
            })
            .filter(|s| agent.as_deref().is_none_or(|a| s.source == a))
            .collect();
        let total = sessions.len();
        sessions.sort_by(|a, b| b.ended.cmp(&a.ended).then_with(|| a.id.cmp(&b.id)));
        sessions.truncate(limit);
        (sessions, total)
    }

    /// Preferred branch for display: the first recorded non-default branch,
    /// falling back to any associated branch entity.
    pub fn primary_branch(&self, id: &str) -> Option<&str> {
        let branches = self.entities_of(id, "branch");
        branches
            .iter()
            .find(|b| b.origin == "recorded" && b.key != "main" && b.key != "master")
            .or(branches.first())
            .map(|b| b.key.as_str())
    }

    pub fn repo_name(&self, s: &SessionMeta) -> Option<String> {
        s.repo_key.as_ref().map(|k| {
            self.repos
                .get(k)
                .cloned()
                .unwrap_or_else(|| crate::extract::repo_display(k))
        })
    }

    /// Sessions associated with an entity, most recently active first. A repo
    /// matches only sessions whose primary repo it is, not mere mentions.
    pub fn entity_sessions(&self, kind: &str, key: &str) -> Vec<String> {
        let mut ids: Vec<String> = if kind == "repo" {
            self.sessions
                .values()
                .filter(|s| s.repo_key.as_deref() == Some(key))
                .map(|s| s.id.clone())
                .collect()
        } else {
            self.by_entity
                .get(&(kind.to_owned(), key.to_lowercase()))
                .cloned()
                .unwrap_or_default()
        };
        ids.sort_by_key(|id| -self.sessions.get(id).map_or(0, |s| s.ended));
        ids.dedup();
        ids
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_sessions_filter_activity_before_applying_the_limit() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        for n in 0..125 {
            c.execute(
                "INSERT INTO sessions (id, source, host, archive_path, started, ended, turn_count)
                 VALUES (?, 'codex', '', '', 0, ?, 1)",
                params![format!("session-{n:03}"), n],
            )
            .unwrap();
        }
        c.execute_batch(
            "INSERT INTO sessions (id, source, host, archive_path, ended, turn_count, skip) VALUES
             ('skipped', 'codex', '', '', 1000, 1, 1),
             ('empty', 'codex', '', '', 1000, 0, 0);",
        )
        .unwrap();

        let cat = Catalog::load(&c).unwrap();
        let (all, total) = cat.recent(None, None, None, 200);
        assert_eq!(total, 125);
        assert_eq!(all.len(), 125);
        assert_eq!(all.first().unwrap().id, "session-124");
        assert_eq!(all.last().unwrap().id, "session-000");
        assert!(all.windows(2).all(|s| s[0].ended > s[1].ended));

        let (recent, total) = cat.recent(None, None, Some(120), 10);
        assert_eq!(total, 5);
        assert_eq!(recent.len(), 5);
        assert_eq!(recent.last().unwrap().ended, 120);
        let (capped, total) = cat.recent(None, None, Some(120), 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(total, 5);
        assert!(cat.recent(None, None, Some(125), 10).0.is_empty());
        assert!(cat.recent(None, None, None, 0).0.is_empty());
    }

    #[test]
    fn recent_filtered_matches_repo_agent_and_reports_total() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(
            "INSERT INTO sessions (id, source, host, archive_path, ended, turn_count, repo_key) VALUES
             ('c1', 'claude', '', '', 300, 2, 'github.com/northwind/billing'),
             ('c2', 'claude', '', '', 200, 1, 'github.com/northwind/billing'),
             ('x1', 'codex', '', '', 250, 4, 'github.com/northwind/widgets'),
             ('x2', 'codex', '', '', 100, 1, 'github.com/northwind/billing');
             INSERT INTO entities (session_id, kind, key, display, origin, strength) VALUES
             ('c1', 'branch', 'main', 'main', 'recorded', 2),
             ('c1', 'branch', 'feat/proration', 'feat/proration', 'recorded', 1),
             ('x1', 'branch', 'main', 'main', 'recorded', 1);",
        )
        .unwrap();

        let cat = Catalog::load(&c).unwrap();
        let (rows, total) = cat.recent(Some("billing"), None, None, 2);
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "c1");
        assert_eq!(rows[1].id, "c2");

        let (rows, total) = cat.recent(Some("northwind/billing"), Some("Claude"), None, 10);
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);

        let (rows, total) = cat.recent(None, Some("codex"), Some(200), 10);
        assert_eq!(total, 1);
        assert_eq!(rows[0].id, "x1");

        assert_eq!(cat.primary_branch("c1"), Some("feat/proration"));
        assert_eq!(cat.primary_branch("x1"), Some("main"));
        assert_eq!(cat.primary_branch("c2"), None);
    }
}
