//! Finds session files, archives them, parses changed ones into SQLite and
//! links sessions to repos.
//!
//! The archive under `<data>/archive` is the long-term record: Claude Code
//! deletes old sessions, and the archive keeps them. Bytes are appended to
//! the archive as the original grows. Changed files are reparsed whole, which
//! takes milliseconds even for large sessions and keeps turn grouping simple.

use crate::config::{Config, data_dir};
use crate::extract;
use crate::model::{Fact, ParsedSession};
use crate::util::{count, hash};
use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Stats {
    pub files: usize,
    pub changed: usize,
    pub sessions: usize,
    pub bad_lines: usize,
    /// One line per stored session, such as `repo "title" +3 turns`, for logs.
    pub updated: Vec<String>,
}

pub fn archive_root() -> PathBuf {
    data_dir().join("archive")
}

/// Source files: (source, absolute path, path relative to the source root).
fn discover(cfg: &Config) -> Vec<(&'static str, PathBuf, PathBuf)> {
    let mut out = vec![];
    let claude = cfg.claude_dir().join("projects");
    walk(&claude, &mut |p| {
        if let Ok(rel) = p.strip_prefix(&claude) {
            out.push(("claude", p.to_path_buf(), Path::new("projects").join(rel)));
        }
    });
    let codex = cfg.codex_dir();
    for sub in ["sessions", "archived_sessions"] {
        let root = codex.join(sub);
        walk(&root, &mut |p| {
            if let Ok(rel) = p.strip_prefix(&codex) {
                out.push(("codex", p.to_path_buf(), rel.to_path_buf()));
            }
        });
    }
    out
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, f),
            Ok(t) if t.is_file() && p.extension().is_some_and(|x| x == "jsonl") => f(&p),
            _ => {}
        }
    }
}

fn prefix_hash(path: &Path) -> Result<String> {
    let mut buf = vec![0u8; 4096];
    let mut f = std::fs::File::open(path)?;
    let n = f.read(&mut buf)?;
    Ok(hash(&[&String::from_utf8_lossy(&buf[..n])]))
}

/// Copies new bytes into the archive. Returns false when nothing changed.
fn sync_archive(
    src: &Path,
    dst: &Path,
    old: Option<&(i64, String)>,
    size: i64,
    prefix: &str,
) -> Result<()> {
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let archived = std::fs::metadata(dst).map(|m| m.len() as i64).ok();
    match (old, archived) {
        (Some((old_size, old_prefix)), Some(a))
            if old_prefix == prefix && size >= *old_size && a == *old_size =>
        {
            let mut from = std::fs::File::open(src)?;
            from.seek(SeekFrom::Start(*old_size as u64))?;
            let mut to = std::fs::OpenOptions::new().append(true).open(dst)?;
            std::io::copy(&mut from.take((size - old_size) as u64), &mut to)?;
            to.flush()?;
        }
        _ => {
            let tmp = dst.with_extension("jsonl.tmp");
            std::fs::copy(src, &tmp)?;
            std::fs::rename(&tmp, dst)?;
        }
    }
    Ok(())
}

pub struct Ingester<'a> {
    cfg: &'a Config,
    host: String,
    tickets: Vec<Regex>,
    worktrees: Vec<Regex>,
}

impl<'a> Ingester<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self {
            cfg,
            host: gethostname::gethostname().to_string_lossy().into_owned(),
            tickets: extract::compile(&cfg.ticket_patterns),
            worktrees: extract::compile(&cfg.worktree_patterns),
        }
    }

    /// One catch-up pass. With `rebuild`, every archived file is reparsed.
    pub fn run(&self, c: &mut Connection, rebuild: bool) -> Result<Stats> {
        let mut stats = Stats::default();
        let known: HashMap<String, (i64, i64, i64, String, String)> = {
            let mut st = c.prepare(
                "SELECT src_path, size, mtime, inode, prefix_hash, archive_path FROM files",
            )?;
            st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        let mut todo: Vec<(&'static str, String, PathBuf)> = vec![];
        for (source, src, rel) in discover(self.cfg) {
            stats.files += 1;
            let Ok(meta) = std::fs::metadata(&src) else {
                continue;
            };
            let key = src.to_string_lossy().into_owned();
            let size = meta.len() as i64;
            let mtime = meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000;
            let inode = meta.ino() as i64;
            let old = known.get(&key);
            if !rebuild && old.is_some_and(|o| o.0 == size && o.1 == mtime && o.2 == inode) {
                continue;
            }
            let dst = archive_root().join(source).join(&rel);
            let prefix = prefix_hash(&src)?;
            // A different inode means the file was replaced, so copy it whole.
            let prev = old.filter(|o| o.2 == inode).map(|o| (o.0, o.3.clone()));
            if old.is_none_or(|o| o.0 != size || o.3 != prefix) || !dst.exists() {
                sync_archive(&src, &dst, prev.as_ref(), size, &prefix)?;
            }
            c.execute(
                "INSERT INTO files (src_path, source, archive_path, size, mtime, inode, prefix_hash) VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(src_path) DO UPDATE SET archive_path = excluded.archive_path, size = excluded.size, mtime = excluded.mtime,
                   inode = excluded.inode, prefix_hash = excluded.prefix_hash",
                params![key, source, dst.to_string_lossy(), size, mtime, inode, prefix],
            )?;
            todo.push((source, key, dst));
        }
        if rebuild {
            // Archived files whose originals are gone still get reparsed.
            let seen: std::collections::HashSet<String> =
                todo.iter().map(|t| t.1.clone()).collect();
            let mut st = c.prepare("SELECT source, src_path, archive_path FROM files")?;
            let rows: Vec<(String, String, String)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            for (source, src, arch) in rows {
                if !seen.contains(&src) && Path::new(&arch).exists() {
                    let source = if source == "claude" {
                        "claude"
                    } else {
                        "codex"
                    };
                    todo.push((source, src, PathBuf::from(arch)));
                }
            }
        }
        stats.changed = todo.len();
        for (source, src, dst) in todo {
            let mut bytes = std::fs::read(&dst)?;
            // Only parse complete lines; the agent may be mid-write.
            let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            bytes.truncate(end);
            let ps = crate::parse::parse(source, &dst, &bytes);
            stats.bad_lines += ps.bad_lines;
            if ps.id.is_empty() {
                continue;
            }
            let facts = extract::extract(&ps, self.cfg, &self.tickets);
            let before: Option<i64> = c
                .query_row(
                    "SELECT turn_count FROM sessions WHERE id = ?",
                    [&ps.id],
                    |r| r.get(0),
                )
                .optional()?;
            if self.store(c, &ps, &facts, &dst, bytes.len() as i64)? {
                c.execute(
                    "UPDATE files SET session_id = ? WHERE src_path = ?",
                    params![ps.id, src],
                )?;
                stats.sessions += 1;
                if !ps.skip {
                    stats.updated.push(self.label(&ps, before));
                }
            }
        }
        self.link_repos(c)?;
        Ok(stats)
    }

    fn label(&self, ps: &ParsedSession, before: Option<i64>) -> String {
        let repo = ps.cwd.as_deref().map_or_else(
            || ps.source.to_string(),
            |cwd| extract::repo_from_path(cwd, &self.worktrees).1,
        );
        let title = ps
            .title
            .as_deref()
            .or_else(|| {
                ps.turns
                    .iter()
                    .map(|t| t.user.as_str())
                    .find(|u| !u.is_empty())
            })
            .map(|t| crate::model::clip_chars(t.lines().next().unwrap_or(""), 60))
            .unwrap_or_else(|| ps.id.chars().take(8).collect());
        let turns = ps.turns.len() as i64;
        let change = match before {
            None => format!("new, {}", count(turns as usize, "turn")),
            Some(b) if turns > b => format!("+{}", count((turns - b) as usize, "turn")),
            Some(_) => "updated".into(),
        };
        format!("{repo} \"{title}\" {change}")
    }

    fn store(
        &self,
        c: &mut Connection,
        ps: &ParsedSession,
        facts: &[Fact],
        archive: &Path,
        bytes: i64,
    ) -> Result<bool> {
        let archive = archive.to_string_lossy();
        // Codex can move a session to archived_sessions; keep the fuller copy.
        let existing: Option<(String, i64)> = c
            .query_row(
                "SELECT archive_path, bytes FROM sessions WHERE id = ?",
                [&ps.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((path, b)) = existing
            && path != archive
            && b > bytes
        {
            return Ok(false);
        }
        let title_is_prompt = ps.title.is_none();
        let title = ps.title.clone().or_else(|| {
            ps.turns
                .iter()
                .find(|t| !t.user.is_empty())
                .map(|t| crate::model::clip_chars(t.user.lines().next().unwrap_or(""), 100))
        });
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, source, parent_id, host, cwd, title, recap, model, started, ended, turn_count, archive_path, bytes, skip, bad_lines, title_is_prompt, dirty)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 1)
             ON CONFLICT(id) DO UPDATE SET source = ?2, parent_id = ?3, host = ?4, cwd = ?5, title = ?6, recap = ?7, model = ?8, started = ?9,
               ended = ?10, turn_count = ?11, archive_path = ?12, bytes = ?13, skip = ?14, bad_lines = ?15, title_is_prompt = ?16, dirty = 1",
            params![
                ps.id,
                ps.source,
                ps.parent_id,
                self.host,
                ps.cwd,
                title,
                ps.recap,
                ps.model,
                ps.started.map(|t| t.timestamp()),
                ps.ended.map(|t| t.timestamp()),
                if ps.skip { 0 } else { ps.turns.len() as i64 },
                archive,
                bytes,
                ps.skip,
                ps.bad_lines as i64,
                title_is_prompt
            ],
        )?;
        tx.execute("DELETE FROM turns WHERE session_id = ?", [&ps.id])?;
        tx.execute("DELETE FROM entities WHERE session_id = ?", [&ps.id])?;
        tx.execute("DELETE FROM blocks WHERE session_id = ?", [&ps.id])?;
        if !ps.skip {
            let mut ins = tx.prepare(
                "INSERT INTO turns (session_id, idx, ts, branch, cwd, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            let mut blk =
                tx.prepare("INSERT OR IGNORE INTO blocks (hash, session_id) VALUES (?, ?)")?;
            for (i, t) in ps.turns.iter().enumerate() {
                let mut tool_text = String::new();
                let mut commands = vec![];
                let mut errors = vec![];
                let mut files: Vec<&str> = vec![];
                for tc in &t.tools {
                    tool_text.push_str(&tc.name);
                    tool_text.push_str(": ");
                    tool_text.push_str(&tc.input);
                    tool_text.push('\n');
                    tool_text.push_str(&tc.output);
                    tool_text.push('\n');
                    if matches!(tc.name.as_str(), "Bash" | "exec" | "shell" | "exec_command") {
                        commands.push(tc.input.as_str());
                    }
                    files.extend(tc.files.iter().map(String::as_str));
                    if tc.is_error {
                        errors.extend(tc.output.lines().take(5));
                    } else {
                        errors.extend(
                            tc.output
                                .lines()
                                .filter(|l| {
                                    l.contains("error")
                                        || l.contains("Error")
                                        || l.contains("panicked")
                                })
                                .take(3),
                        );
                    }
                }
                let tools_json = serde_json::to_string(
                    &t.tools.iter().map(|tc| serde_json::json!({"name": tc.name, "input": tc.input, "output": tc.output, "error": tc.is_error})).collect::<Vec<_>>(),
                )?;
                let ts = t.ts.map(|x| x.timestamp());
                // The same prompt at the same time in two sessions is a fork or a replay.
                let ukey = if t.user.is_empty() {
                    hash(&[&ps.id, &i.to_string()])
                } else {
                    hash(&[&t.user, &ts.unwrap_or(0).to_string()])
                };
                let chash = hash(&[&t.user, &t.assistant]);
                ins.execute(params![
                    ps.id,
                    i as i64,
                    ts,
                    t.branch,
                    t.cwd,
                    t.user,
                    t.assistant,
                    tool_text,
                    files.join("\n"),
                    commands.join("\n"),
                    errors.join("\n"),
                    tools_json,
                    ukey,
                    chash
                ])?;
                for p in paragraphs(&t.user) {
                    blk.execute(params![hash(&[&p]), ps.id])?;
                }
            }
            let mut ent = tx.prepare("INSERT OR REPLACE INTO entities (session_id, kind, key, display, origin, strength) VALUES (?, ?, ?, ?, ?, ?)")?;
            for f in facts {
                ent.execute(params![
                    ps.id, f.kind, f.key, f.display, f.origin, f.strength
                ])?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Assigns each session a repo. A remote the session itself recorded or
    /// pushed to wins. Otherwise another session in the same directory may
    /// know the remote. Otherwise the repo is named from the path, and that
    /// name merges with a known remote of the same name when there is
    /// exactly one.
    fn link_repos(&self, c: &mut Connection) -> Result<()> {
        let sessions: Vec<(String, Option<String>, Option<String>, Option<String>)> = {
            let mut st =
                c.prepare("SELECT id, cwd, repo_key, repo_origin FROM sessions WHERE skip = 0")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<Result<_, _>>()?
        };
        // Every repo tied for the strongest evidence, per session.
        let mut top: HashMap<String, (i64, String, Vec<String>)> = HashMap::new();
        {
            let mut st = c.prepare(
                "SELECT session_id, key, origin, strength FROM entities WHERE kind = 'repo' AND strength >= 2 ORDER BY session_id, strength DESC, origin DESC, key",
            )?;
            for row in st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })? {
                let (sid, key, origin, strength) = row?;
                let e = top.entry(sid).or_insert((strength, origin.clone(), vec![]));
                if e.0 == strength && e.1 == origin {
                    e.2.push(key);
                }
            }
        }
        // A session can open PRs in several repos; the one it ran in is primary.
        let cwds: HashMap<&str, &str> = sessions
            .iter()
            .filter_map(|(sid, cwd, _, _)| Some((sid.as_str(), cwd.as_deref()?)))
            .collect();
        let best: HashMap<String, (String, String)> = top
            .into_iter()
            .map(|(sid, (_, origin, keys))| {
                let key =
                    extract::primary_repo(&keys, cwds.get(sid.as_str()).copied(), &self.worktrees)
                        .to_string();
                (sid, (key, origin))
            })
            .collect();
        let tx = c.transaction()?;
        for (sid, cwd, _, _) in &sessions {
            if let (Some((key, _)), Some(cwd)) = (best.get(sid), cwd) {
                tx.execute(
                    "INSERT OR REPLACE INTO cwd_repo (cwd, repo_key) VALUES (?, ?)",
                    params![cwd, key],
                )?;
                if let Some(root) = self.worktree_root(cwd) {
                    tx.execute(
                        "INSERT OR IGNORE INTO cwd_repo (cwd, repo_key) VALUES (?, ?)",
                        params![root, key],
                    )?;
                }
            }
        }
        let cwd_repo: Vec<(String, String)> = {
            let mut st =
                tx.prepare("SELECT cwd, repo_key FROM cwd_repo ORDER BY length(cwd) DESC")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (_, key) in &cwd_repo {
            let name = key.rsplit('/').next().unwrap_or(key).to_string();
            let v = by_name.entry(name).or_default();
            if !v.contains(key) {
                v.push(key.clone());
            }
        }
        for (sid, cwd, old_key, old_origin) in sessions {
            let (key, origin) = if let Some((k, o)) = best.get(&sid) {
                (k.clone(), o.clone())
            } else if let Some(cwd) = &cwd {
                if let Some((_, k)) = cwd_repo
                    .iter()
                    .find(|(p, _)| cwd == p || cwd.starts_with(&format!("{p}/")))
                {
                    (k.clone(), "inferred".into())
                } else {
                    let (pk, name) = extract::repo_from_path(cwd, &self.worktrees);
                    match by_name.get(&name.to_lowercase()) {
                        Some(v) if v.len() == 1 => (v[0].clone(), "inferred".into()),
                        _ => (pk, "inferred".into()),
                    }
                }
            } else {
                continue;
            };
            if old_key.as_deref() != Some(key.as_str())
                || old_origin.as_deref() != Some(origin.as_str())
            {
                tx.execute(
                    "UPDATE sessions SET repo_key = ?, repo_origin = ?, dirty = 1 WHERE id = ?",
                    params![key, origin, sid],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn worktree_root(&self, cwd: &str) -> Option<String> {
        self.worktrees.iter().find_map(|re| {
            re.captures(cwd)
                .and_then(|c| c.name("root"))
                .map(|m| m.as_str().to_string())
        })
    }
}

/// Paragraphs long enough to be boilerplate if they repeat across sessions.
pub fn paragraphs(text: &str) -> Vec<String> {
    text.split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| p.len() >= 80)
        .collect()
}
