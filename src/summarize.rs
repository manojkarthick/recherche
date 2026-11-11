//! Session summaries. Each idle session is summarized once from a capped
//! digest, not the full transcript, and again only after it grows a lot.

use crate::config::Config;
use crate::llm::Llm;
use crate::model::clip_chars;
use crate::store;
use anyhow::Result;
use rusqlite::{params, Connection};

const DIGEST_CHARS: usize = 22_000;

pub struct Candidate {
    pub id: String,
    pub turns: i64,
    pub digest: String,
}

/// Sessions due for a summary. Without `backfill`, only sessions that
/// started after the daemon first ran are picked; older ones wait for an
/// explicit `recherche summarize --all`.
pub fn candidates(c: &Connection, cfg: &Config, backfill: bool) -> Result<Vec<Candidate>> {
    let cutoff: i64 = if backfill { 0 } else { store::get_meta(c, "auto_summary_since")?.and_then(|v| v.parse().ok()).unwrap_or(i64::MAX) };
    let idle_before = chrono::Utc::now().timestamp() - cfg.llm.idle_minutes * 60;
    let mut st = c.prepare(
        "SELECT id, source, COALESCE(cwd, ''), turn_count, summary_turns FROM sessions
         WHERE skip = 0 AND turn_count > 0 AND parent_id IS NULL AND ended < ? AND started >= ?
         ORDER BY ended DESC",
    )?;
    let rows: Vec<(String, String, String, i64, Option<i64>)> =
        st.query_map(params![idle_before, cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
    let mut out = vec![];
    for (id, source, cwd, turns, done) in rows {
        if !cfg.llm.summarize_sources.contains(&source) || cfg.llm.summarize_exclude_paths.iter().any(|p| !p.is_empty() && cwd.contains(p.as_str())) {
            continue;
        }
        // Re-summarize only once a session has grown by half since its summary.
        if done.is_some_and(|d| turns < d + d / 2 + 2) {
            continue;
        }
        out.push(Candidate { digest: digest(c, &id)?, id, turns });
    }
    Ok(out)
}

/// What the summary is written from: prompts, commits, PRs, files, errors
/// and the final reply. Most of a session's signal is in these.
pub fn digest(c: &Connection, id: &str) -> Result<String> {
    let (title, repo, cwd, source): (Option<String>, Option<String>, Option<String>, String) =
        c.query_row("SELECT title, repo_key, cwd, source FROM sessions WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    let mut out = format!(
        "Agent: {source}\nTitle: {}\nRepo: {}\nDirectory: {}\n",
        title.unwrap_or_default(),
        repo.map(|r| crate::extract::repo_display(&r)).unwrap_or_default(),
        cwd.unwrap_or_default()
    );
    let mut st = c.prepare("SELECT kind, display FROM entities WHERE session_id = ? AND kind IN ('pr', 'branch', 'ticket', 'commit') ORDER BY kind")?;
    for row in st.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
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
    let errors: Vec<&str> = rows.iter().flat_map(|r| r.2.lines()).filter(|l| !l.trim().is_empty()).take(10).collect();
    if !errors.is_empty() {
        out.push_str(&format!("Errors seen:\n{}\n", errors.iter().map(|e| clip_chars(e, 200)).collect::<Vec<_>>().join("\n")));
    }
    let last = rows.iter().rev().find(|r| !r.1.is_empty()).map(|r| clip_chars(&r.1, 1500)).unwrap_or_default();
    out.push_str("\nUser prompts, in order:\n");
    let budget = DIGEST_CHARS.saturating_sub(out.len() + last.len() + 100);
    let prompts: Vec<&String> = rows.iter().map(|r| &r.0).filter(|u| !u.is_empty()).collect();
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

/// Summarizes up to `limit` candidates. Stops at the budget or on the first
/// API failure, because the next call would most likely fail the same way.
/// With `progress`, prints one line per session to stderr (for the CLI).
pub async fn run(llm: &Llm, cfg: &Config, db: &std::path::Path, backfill: bool, limit: usize, progress: bool) -> Result<usize> {
    if !llm.summaries_enabled() {
        return Ok(0);
    }
    let todo = {
        let c = store::open(db)?;
        candidates(&c, cfg, backfill)?
    };
    let total = todo.len().min(limit);
    let started = std::time::Instant::now();
    let mut n = 0;
    for (i, cand) in todo.into_iter().take(limit).enumerate() {
        match llm.summarize(&cand.digest).await {
            Ok(s) => {
                if progress {
                    eprintln!(
                        "[{}/{total}] {}  (${:.4} spent this month, {}s elapsed)",
                        i + 1,
                        clip_chars(&s.title, 70),
                        llm.spent(),
                        started.elapsed().as_secs()
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
                tracing::warn!("[{}/{total}] summary for {} failed, stopping: {e:#}", i + 1, cand.id);
                break;
            }
        }
    }
    Ok(n)
}
