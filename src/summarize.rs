//! Session summaries. Each idle session is summarized once from a capped
//! digest, not the full transcript, and again only after it grows a lot.

use crate::config::Config;
use crate::llm::Llm;
use crate::model::clip_chars;
use crate::store;
use anyhow::Result;
use rusqlite::{Connection, params};

const DIGEST_CHARS: usize = 22_000;

pub struct Candidate {
    pub id: String,
    /// The repo name, for logs.
    pub repo: String,
    pub turns: i64,
    pub digest: String,
}

/// Sessions due for a summary, most recently ended first. Without `backfill`
/// (the daemon), only sessions that started after the daemon first ran are
/// picked; older ones wait for an explicit `recherche summarize`. `force`
/// bypasses the inactivity and growth checks.
pub fn candidates(
    c: &Connection,
    cfg: &Config,
    backfill: bool,
    force: bool,
) -> Result<Vec<Candidate>> {
    let cutoff: i64 = if backfill {
        0
    } else {
        store::get_meta(c, "auto_summary_since")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(i64::MAX)
    };
    let idle_before = chrono::Utc::now().timestamp() - cfg.llm.idle_minutes * 60;
    let mut st = c.prepare(
        "SELECT id, source, COALESCE(cwd, ''), turn_count, summary_turns, COALESCE(repo_key, '') FROM sessions
         WHERE skip = 0 AND turn_count > 0 AND parent_id IS NULL AND (? OR ended < ?) AND started >= ?
         ORDER BY ended DESC",
    )?;
    let rows: Vec<(String, String, String, i64, Option<i64>, String)> = st
        .query_map(params![force, idle_before, cutoff], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (id, source, cwd, turns, done, repo) in rows {
        if !allowed(cfg, &source, &cwd) {
            continue;
        }
        // Re-summarize only once a session has grown by half since its summary.
        if !force && done.is_some_and(|d| turns < d + d / 2 + 2) {
            continue;
        }
        out.push(Candidate {
            digest: digest(c, &id)?,
            repo: if repo.is_empty() {
                source
            } else {
                crate::extract::repo_display(&repo)
            },
            id,
            turns,
        });
    }
    Ok(out)
}

/// Whether the config lets this session be sent for a summary at all.
fn allowed(cfg: &Config, source: &str, cwd: &str) -> bool {
    cfg.llm.summarize_sources.iter().any(|s| s == source)
        && !cfg
            .llm
            .summarize_exclude_paths
            .iter()
            .any(|p| !p.is_empty() && cwd.contains(p.as_str()))
}

/// Why a session the daemon would summarize has no summary yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pending {
    /// Active within `idle_minutes`; summarized once it goes quiet.
    InProgress,
    /// Idle and due; the daemon picks it up on its next pass.
    Queued,
    /// Started before the daemon first ran, so only `recherche summarize` covers it.
    Backfill,
}

/// Mirrors the filters in `candidates`, assuming summaries are enabled.
/// `None` when the session has a summary or recap, or will never get one
/// (a subagent, or excluded by source or path).
pub fn pending(cfg: &Config, since: i64, s: &store::SessionMeta, now: i64) -> Option<Pending> {
    if s.summary.is_some()
        || s.recap.is_some()
        || s.parent_id.is_some()
        || !allowed(cfg, &s.source, s.cwd.as_deref().unwrap_or(""))
    {
        return None;
    }
    Some(if s.started < since {
        Pending::Backfill
    } else if s.ended >= now - cfg.llm.idle_minutes * 60 {
        Pending::InProgress
    } else {
        Pending::Queued
    })
}

/// What the summary is written from: prompts, commits, PRs, files, errors
/// and the final reply. Most of a session's signal is in these.
pub fn digest(c: &Connection, id: &str) -> Result<String> {
    let (title, repo, cwd, source): (Option<String>, Option<String>, Option<String>, String) = c
        .query_row(
            "SELECT title, repo_key, cwd, source FROM sessions WHERE id = ?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let mut out = format!(
        "Agent: {source}\nTitle: {}\nRepo: {}\nDirectory: {}\n",
        title.unwrap_or_default(),
        repo.map(|r| crate::extract::repo_display(&r))
            .unwrap_or_default(),
        cwd.unwrap_or_default()
    );
    let mut st = c.prepare("SELECT kind, display FROM entities WHERE session_id = ? AND kind IN ('pr', 'branch', 'ticket', 'commit') ORDER BY kind")?;
    for row in st.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (k, d) = row?;
        out.push_str(&format!("{k}: {d}\n"));
    }
    let files: Vec<String> = c
        .prepare("SELECT display FROM entities WHERE session_id = ? AND kind = 'file' LIMIT 20")?
        .query_map([id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !files.is_empty() {
        out.push_str(&format!("Files: {}\n", files.join(", ")));
    }
    let rows: Vec<(String, String, String)> = c
        .prepare("SELECT COALESCE(clean_user, user_text), assistant_text, errors FROM turns WHERE session_id = ? AND dup = 0 ORDER BY idx")?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let errors: Vec<&str> = rows
        .iter()
        .flat_map(|r| r.2.lines())
        .filter(|l| !l.trim().is_empty())
        .take(10)
        .collect();
    if !errors.is_empty() {
        out.push_str(&format!(
            "Errors seen:\n{}\n",
            errors
                .iter()
                .map(|e| clip_chars(e, 200))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    let last = rows
        .iter()
        .rev()
        .find(|r| !r.1.is_empty())
        .map(|r| clip_chars(&r.1, 1500))
        .unwrap_or_default();
    out.push_str("\nUser prompts, in order:\n");
    let budget = DIGEST_CHARS.saturating_sub(out.len() + last.len() + 100);
    let prompts: Vec<&String> = rows
        .iter()
        .map(|r| &r.0)
        .filter(|u| !u.is_empty())
        .collect();
    let per = (budget / prompts.len().max(1)).clamp(120, 600);
    let mut used = 0;
    for p in &prompts {
        let p = clip_chars(p, per);
        if used + p.len() > budget {
            out.push_str("- …\n");
            break;
        }
        used += p.len();
        out.push_str(&format!("- {}\n", p.replace('\n', " ")));
    }
    out.push_str(&format!("\nFinal assistant message:\n{last}\n"));
    Ok(out)
}

/// Summarizes up to `limit` candidates. Stops on the first API failure, because the next call would most likely fail the same way.
/// With `progress`, prints one line per session to stderr (for the CLI).
pub async fn run(
    llm: &Llm,
    cfg: &Config,
    db: &std::path::Path,
    backfill: bool,
    force: bool,
    limit: usize,
    progress: bool,
) -> Result<usize> {
    if !llm.summaries_enabled() {
        return Ok(0);
    }
    let todo = {
        let c = store::open(db)?;
        candidates(&c, cfg, backfill, force)?
    };
    let total = todo.len().min(limit);
    let started = std::time::Instant::now();
    let mut n = 0;
    for (i, cand) in todo.into_iter().take(limit).enumerate() {
        match llm.summarize(&cand.digest).await {
            Ok(s) => {
                if progress {
                    eprintln!(
                        "[{}/{total}] {}  ({}s elapsed)",
                        i + 1,
                        clip_chars(&s.title, 70),
                        started.elapsed().as_secs()
                    );
                } else {
                    tracing::info!(
                        "summarized {} \"{}\" ({}, {}/{total})",
                        cand.repo,
                        clip_chars(&s.title, 60),
                        crate::util::count(cand.turns as usize, "turn"),
                        i + 1
                    );
                }
                let c = store::open(db)?;
                c.execute(
                    "UPDATE sessions SET summary = ?, llm_title = ?, kind = ?, outcome = ?, tags = ?, summary_turns = ?, dirty = 1 WHERE id = ?",
                    params![s.summary, s.title, s.kind, s.outcome, s.tags.join(", "), cand.turns, cand.id],
                )?;
                n += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "[{}/{total}] summary for {} failed, stopping: {e:#}",
                    i + 1,
                    cand.id
                );
                break;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_explains_why_a_session_has_no_summary() {
        let now = 100_000;
        let since = 50_000;
        let mut cfg = Config::default();
        cfg.llm.summarize_sources = vec!["claude".into()];
        cfg.llm.summarize_exclude_paths = vec!["/excluded".into()];
        cfg.llm.idle_minutes = 20;
        let base = store::SessionMeta {
            source: "claude".into(),
            started: since + 1,
            ended: now - 60,
            ..Default::default()
        };
        let with = |f: fn(&mut store::SessionMeta)| {
            let mut s = base.clone();
            f(&mut s);
            pending(&cfg, since, &s, now)
        };
        assert_eq!(with(|_| {}), Some(Pending::InProgress));
        assert_eq!(with(|s| s.ended = 100_000 - 21 * 60), Some(Pending::Queued));
        assert_eq!(with(|s| s.started = 49_999), Some(Pending::Backfill));
        assert_eq!(with(|s| s.summary = Some("- Done".into())), None);
        assert_eq!(with(|s| s.recap = Some("Recap".into())), None);
        assert_eq!(with(|s| s.parent_id = Some("parent".into())), None);
        assert_eq!(with(|s| s.source = "codex".into()), None);
        assert_eq!(with(|s| s.cwd = Some("/excluded/repo".into())), None);
    }

    #[test]
    fn force_bypasses_inactivity_and_growth_but_preserves_exclusions() -> Result<()> {
        let c = store::open(std::path::Path::new(":memory:"))?;
        let now = chrono::Utc::now().timestamp();
        store::set_meta(&c, "auto_summary_since", &(now - 10_800).to_string())?;
        let mut cfg = Config::default();
        cfg.llm.summarize_sources = vec!["claude".into()];
        cfg.llm.summarize_exclude_paths = vec!["/excluded".into()];
        for id in [
            "unchanged",
            "grown",
            "new",
            "active",
            "active_new",
            "excluded",
            "source",
            "child",
            "skipped",
            "empty",
            "older",
        ] {
            c.execute(
                "INSERT INTO sessions (id, source, host, cwd, started, ended, turn_count, summary_turns, summary, archive_path)
                 VALUES (?, 'claude', 'test', '/project', ?, ?, 10, 10, 'Existing summary', '')",
                params![id, now - 7200, now - 3600],
            )?;
        }
        c.execute_batch(
            "UPDATE sessions SET turn_count = 17 WHERE id = 'grown';
             UPDATE sessions SET summary_turns = NULL, summary = NULL WHERE id IN ('new', 'active_new');
             UPDATE sessions SET cwd = '/excluded/project' WHERE id = 'excluded';
             UPDATE sessions SET source = 'codex' WHERE id = 'source';
             UPDATE sessions SET parent_id = 'unchanged' WHERE id = 'child';
             UPDATE sessions SET skip = 1 WHERE id = 'skipped';
             UPDATE sessions SET turn_count = 0 WHERE id = 'empty';",
        )?;
        c.execute(
            "UPDATE sessions SET ended = ? WHERE id IN ('active', 'active_new')",
            [now],
        )?;
        c.execute(
            "UPDATE sessions SET started = ? WHERE id = 'older'",
            [now - 14_400],
        )?;
        let ids = |backfill, force| -> Result<Vec<String>> {
            let mut ids: Vec<_> = candidates(&c, &cfg, backfill, force)?
                .into_iter()
                .map(|s| s.id)
                .collect();
            ids.sort();
            Ok(ids)
        };
        assert_eq!(ids(false, false)?, ["grown", "new"]);
        assert_eq!(ids(true, false)?, ["grown", "new"]);
        assert_eq!(
            ids(false, true)?,
            ["active", "active_new", "grown", "new", "unchanged"]
        );
        assert_eq!(
            ids(true, true)?,
            ["active", "active_new", "grown", "new", "older", "unchanged"]
        );
        let newest: Vec<_> = candidates(&c, &cfg, true, true)?
            .into_iter()
            .take(2)
            .map(|s| s.id)
            .collect();
        assert!(newest.contains(&"active".to_string()));
        assert!(newest.contains(&"active_new".to_string()));
        let stored: (String, i64) = c.query_row(
            "SELECT summary, summary_turns FROM sessions WHERE id = 'unchanged'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(stored, ("Existing summary".into(), 10));
        Ok(())
    }
}
