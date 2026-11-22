//! Session summaries. Each idle session is summarized once from a capped
//! digest, not the full transcript, and again only after it grows a lot.

use crate::config::Config;
use crate::llm::{Llm, Summary};
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

/// Whether this session is eligible for summarization under the current config
/// (top-level session, allowed source, and not under an excluded path).
pub fn eligible(cfg: &Config, s: &store::SessionMeta) -> bool {
    s.parent_id.is_none() && allowed(cfg, &s.source, s.cwd.as_deref().unwrap_or(""))
}

/// Mirrors the filters in `candidates`, assuming summaries are enabled.
/// `None` when the session has a summary or recap, or will never get one
/// (a subagent, or excluded by source or path).
pub fn pending(cfg: &Config, since: i64, s: &store::SessionMeta, now: i64) -> Option<Pending> {
    if s.summary.is_some() || s.recap.is_some() || !eligible(cfg, s) {
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

/// What the summary is written from: prompts, commits, PRs, files, errors,
/// selected test results and the final reply.
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
    let rows: Vec<(String, String, String, String)> = c
        .prepare("SELECT COALESCE(clean_user, user_text), assistant_text, errors, tools_json FROM turns WHERE session_id = ? AND dup = 0 ORDER BY idx")?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
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
    let mut test_results = Vec::new();
    for row in &rows {
        let tools: Vec<serde_json::Value> = serde_json::from_str(&row.3)?;
        for tool in tools {
            if !tool["name"]
                .as_str()
                .is_some_and(|n| crate::model::COMMAND_TOOLS.contains(&n))
            {
                continue;
            }
            for line in tool["output"].as_str().unwrap_or("").lines() {
                let line = line.trim();
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("pass ")
                    || lower.starts_with("fail ")
                    || lower.starts_with("test result:")
                    || lower.contains(" passed")
                    || lower.contains(" failed")
                {
                    test_results.push(clip_chars(line, 200));
                }
            }
        }
    }
    if !test_results.is_empty() {
        // Recent successful reruns must not be hidden behind earlier failures.
        out.push_str("Test results in order (later results may resolve earlier failures):\n");
        out.push_str(&test_results[test_results.len().saturating_sub(10)..].join("\n"));
        out.push('\n');
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

/// Stores a summary written from the session's first `turns` turns. The next
/// index pass picks it up.
pub fn save(c: &Connection, id: &str, s: &Summary, turns: i64) -> Result<()> {
    c.execute(
        "UPDATE sessions SET summary = ?, llm_title = ?, kind = ?, outcome = ?, tags = ?, summary_turns = ?, dirty = 1 WHERE id = ?",
        params![s.summary, s.title, s.kind, s.outcome, s.tags.join(", "), turns, id],
    )?;
    Ok(())
}

/// Summarizes one session on demand, bypassing inactivity and growth checks
/// while still honoring `summaries_enabled`, source/path exclusions, and
/// top-level session rules.
pub async fn summarize_session(
    llm: &Llm,
    cfg: &Config,
    db: &std::path::Path,
    id: &str,
) -> Result<Summary> {
    use rusqlite::OptionalExtension;

    if !llm.summaries_enabled() {
        anyhow::bail!(
            "Summaries are off: set {} and keep llm.summaries = true.",
            cfg.llm.api_key_env
        );
    }
    let cand = {
        let c = store::open(db)?;
        let row: Option<(String, String, i64, Option<String>, bool, String)> = c
            .query_row(
                "SELECT source, COALESCE(cwd, ''), turn_count, parent_id, skip, COALESCE(repo_key, '')
                 FROM sessions WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let Some((source, cwd, turns, parent_id, skip, repo)) = row else {
            anyhow::bail!("No session with that ID.");
        };
        if skip || turns <= 0 {
            anyhow::bail!("Session has no searchable turns to summarize.");
        }
        if parent_id.is_some() {
            anyhow::bail!("Subagent sessions are not summarized.");
        }
        if !allowed(cfg, &source, &cwd) {
            anyhow::bail!("This session is excluded from summaries by configuration.");
        }
        Candidate {
            digest: digest(&c, id)?,
            repo: if repo.is_empty() {
                source
            } else {
                crate::extract::repo_display(&repo)
            },
            id: id.to_string(),
            turns,
        }
    };
    let s = llm.summarize(&cand.digest).await?;
    tracing::info!(
        "summarized {} \"{}\" ({}, on demand)",
        cand.repo,
        clip_chars(&s.title, 60),
        crate::util::count(cand.turns as usize, "turn"),
    );
    save(&store::open(db)?, &cand.id, &s, cand.turns)?;
    Ok(s)
}

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub backfill: bool,
    pub force: bool,
    pub limit: usize,
    pub progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResult {
    pub summarized: usize,
    pub total: usize,
    pub cancelled: bool,
    pub error: Option<String>,
}

/// Summarizes all eligible sessions needing a summary (including older
/// backfill sessions), reporting progress and stopping cleanly on cancel or
/// API failure. Used by the web UI's on-demand "Summarize all" task.
pub async fn run_batch(
    llm: &Llm,
    cfg: &Config,
    db: &std::path::Path,
    mut on_progress: impl FnMut(usize, usize),
    mut on_saved: impl FnMut(),
    is_cancelled: impl Fn() -> bool,
) -> Result<BatchResult> {
    if !llm.summaries_enabled() {
        anyhow::bail!(
            "Summaries are off: set {} and keep llm.summaries = true.",
            cfg.llm.api_key_env
        );
    }
    let todo = {
        let c = store::open(db)?;
        candidates(&c, cfg, true, false)?
    };
    let total = todo.len();
    let mut summarized = 0;
    for (i, cand) in todo.into_iter().enumerate() {
        if is_cancelled() {
            return Ok(BatchResult {
                summarized,
                total,
                cancelled: true,
                error: None,
            });
        }
        on_progress(i + 1, total);
        match llm.summarize(&cand.digest).await {
            Ok(s) => {
                tracing::info!(
                    "summarized {} \"{}\" ({}, {}/{total}, on demand)",
                    cand.repo,
                    clip_chars(&s.title, 60),
                    crate::util::count(cand.turns as usize, "turn"),
                    i + 1
                );
                save(&store::open(db)?, &cand.id, &s, cand.turns)?;
                summarized += 1;
                on_saved();
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(
                    "[{}/{total}] summary for {} failed, stopping: {msg}",
                    i + 1,
                    cand.id
                );
                return Ok(BatchResult {
                    summarized,
                    total,
                    cancelled: false,
                    error: Some(msg),
                });
            }
        }
    }
    Ok(BatchResult {
        summarized,
        total,
        cancelled: false,
        error: None,
    })
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
    run_until(
        llm,
        cfg,
        db,
        RunOptions {
            backfill,
            force,
            limit,
            progress,
        },
        || false,
    )
    .await
}

/// Like `run`, but checks `stop()` before each session so the daemon can yield
/// immediately to user-requested tasks.
pub async fn run_until(
    llm: &Llm,
    cfg: &Config,
    db: &std::path::Path,
    opts: RunOptions,
    stop: impl Fn() -> bool,
) -> Result<usize> {
    if !llm.summaries_enabled() {
        return Ok(0);
    }
    let todo = {
        let c = store::open(db)?;
        candidates(&c, cfg, opts.backfill, opts.force)?
    };
    let total = todo.len().min(opts.limit);
    let started = std::time::Instant::now();
    let mut n = 0;
    for (i, cand) in todo.into_iter().take(opts.limit).enumerate() {
        if stop() {
            break;
        }
        match llm.summarize(&cand.digest).await {
            Ok(s) => {
                if opts.progress {
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
                save(&store::open(db)?, &cand.id, &s, cand.turns)?;
                n += 1;
            }
            Err(e) => {
                return Err(e.context(format!(
                    "summary for {} failed after {n}/{total} sessions",
                    cand.id
                )));
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn summary_request_preserves_failed_and_later_passing_tests() -> Result<()> {
        use axum::{Json, Router, routing::post};
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(String::new()));
        let captured_request = captured.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = Router::new().route(
            "/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| async move {
                *captured_request.lock().unwrap() = body["messages"][1]["content"]
                    .as_str().unwrap().to_string();
                Json(serde_json::json!({"choices": [{"message": {"content": "{\"title\":\"Fixed\",\"summary\":\"- Fixed.\"}"}}]}))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("recherche.db");
        let c = store::open(&db)?;
        c.execute("INSERT INTO sessions (id, source, host, title, started, ended, turn_count, archive_path) VALUES ('s1', 'codex', '', 'Sync', 1, 2, 1, '')", [])?;
        let tools = serde_json::json!([
            {"name":"exec", "output":"FAIL sync.test.ts\nexpected 409, got 200", "error":true},
            {"name":"exec", "output":"PASS sync.test.ts (14 tests)", "error":false}
        ])
        .to_string();
        c.execute("INSERT INTO turns (session_id, idx, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash) VALUES ('s1', 0, 'fix sync', 'committed', '', '', '', 'FAIL sync.test.ts', ?, 'u', 'c')", [tools])?;
        let mut cfg = Config::default();
        cfg.llm.api_key_env = "HOME".into();
        cfg.llm.base_url = format!("http://{addr}");
        let llm = Llm::new(cfg.llm.clone(), db.clone());
        summarize_session(&llm, &cfg, &db, "s1").await?;
        let digest = captured.lock().unwrap();
        let results = digest.split("Test results in order").nth(1).unwrap();
        assert!(
            results.find("FAIL sync.test.ts").unwrap()
                < results.find("PASS sync.test.ts (14 tests)").unwrap(),
            "{digest}"
        );
        server.abort();
        Ok(())
    }

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

    #[tokio::test]
    async fn summarize_session_honors_exclusions_and_bypasses_idle_check() -> Result<()> {
        use axum::routing::post;
        use axum::{Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = Router::new().route(
            "/chat/completions",
            post(|| async {
                Json(serde_json::json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {
                            "content": serde_json::json!({
                                "title": "On-demand title",
                                "summary": "- Summarized on demand.",
                                "kind": "bugfix",
                                "outcome": "fixed",
                                "tags": ["queue"]
                            }).to_string()
                        }
                    }],
                    "usage": {"completion_tokens": 25}
                }))
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir()?;
        let db = dir.path().join("recherche.db");
        let c = store::open(&db)?;
        let now = chrono::Utc::now().timestamp();
        store::set_meta(&c, "auto_summary_since", &(now + 3600).to_string())?;
        for (id, source, cwd, parent) in [
            ("active", "claude", "/work/app", None),
            ("excluded", "claude", "/secret/app", None),
            ("sub", "claude", "/work/app", Some("active")),
            ("codex_off", "codex", "/work/app", None),
        ] {
            c.execute(
                "INSERT INTO sessions (id, source, parent_id, host, cwd, title, started, ended, turn_count, archive_path, dirty)
                 VALUES (?, ?, ?, 'test', ?, 'Prompt', ?, ?, 2, '', 0)",
                params![id, source, parent, cwd, now - 60, now],
            )?;
            c.execute(
                "INSERT INTO turns (session_id, idx, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash)
                 VALUES (?, 0, 'fix the bug', 'done', '', '', '', '', '[]', ?, ?)",
                params![id, id, id],
            )?;
        }

        let mut cfg = Config::default();
        cfg.llm.enabled = true;
        cfg.llm.summaries = true;
        cfg.llm.api_key_env = "HOME".into();
        cfg.llm.base_url = format!("http://{addr}");
        cfg.llm.summarize_sources = vec!["claude".into()];
        cfg.llm.summarize_exclude_paths = vec!["/secret".into()];
        let llm = Llm::new(cfg.llm.clone(), db.clone());

        assert!(
            summarize_session(&llm, &cfg, &db, "excluded")
                .await
                .is_err()
        );
        assert!(summarize_session(&llm, &cfg, &db, "sub").await.is_err());
        assert!(
            summarize_session(&llm, &cfg, &db, "codex_off")
                .await
                .is_err()
        );
        assert!(summarize_session(&llm, &cfg, &db, "missing").await.is_err());

        let s = summarize_session(&llm, &cfg, &db, "active").await?;
        assert_eq!(s.title, "On-demand title");
        let stored: (String, String, i64, i64) = c.query_row(
            "SELECT summary, llm_title, summary_turns, dirty FROM sessions WHERE id = 'active'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert_eq!(
            stored,
            (
                "- Summarized on demand.".into(),
                "On-demand title".into(),
                2,
                1
            )
        );
        server.abort();
        Ok(())
    }
}
