//! The retrieval pipeline.
//!
//! 1. Query understanding (query.rs).
//! 2. Candidates: BM25 over turns and cards, vector search over the same
//!    documents, and sessions that contain an exact entity from the query.
//! 3. Per-session aggregation: best document plus a small bonus for others.
//! 4. Reciprocal rank fusion across the lists, plus boosts for intent, repo,
//!    time and recency. Exact entity hits are pinned first.
//! 5. A cross-encoder reranks the top sessions against the original query.
//! 6. Optionally, an LLM rewrite adds more lists at a lower weight; the
//!    reranker still scores against the user's own words.

use crate::config::Config;
use crate::lexical::{LexHit, Lexical};
use crate::llm::{Llm, Rewrite};
use crate::models::{Models, VecStore};
use crate::query::{self, Parsed};
use crate::queue::TaskQueue;
use crate::store::{self, Catalog, SessionMeta};
use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

const RRF_K: f32 = 60.0;

pub struct Engine {
    pub cfg: Config,
    pub db: PathBuf,
    pub lex: Lexical,
    pub vecs: RwLock<Arc<VecStore>>,
    pub models: Models,
    pub catalog: RwLock<Arc<Catalog>>,
    pub llm: Llm,
    pub tickets: Vec<Regex>,
    pub tasks: TaskQueue,
    refresh_lock: Mutex<()>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    pub vectors: bool,
    pub rerank: bool,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub session: String,
    pub score: f32,
    pub reasons: Vec<String>,
    /// Best matching turn ids, best first.
    pub turns: Vec<i64>,
    pub rerank: Option<f32>,
}

#[derive(Debug, Clone, Default)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub stage: &'static str,
    pub took_ms: u128,
    pub notes: Vec<String>,
    pub rewrite: Option<Rewrite>,
    /// Top rerank probability, used to decide whether a rewrite may help.
    pub confidence: Option<f32>,
}

#[derive(Default)]
struct Agg {
    best: f32,
    rest: f32,
    n: usize,
    turns: Vec<(i64, f32)>,
    card: bool,
}

impl Engine {
    pub fn open(cfg: Config) -> Result<Self> {
        Self::open_in(cfg, &crate::config::data_dir())
    }

    pub(crate) fn open_in(cfg: Config, data: &Path) -> Result<Self> {
        let db = data.join("recherche.db");
        let conn = store::open(&db)?;
        let lex = Lexical::open(&data.join("tantivy"))?;
        let catalog = Catalog::load(&conn)?;
        let vecs = VecStore::load(&conn)?;
        Ok(Self {
            tickets: crate::extract::compile(&cfg.ticket_patterns),
            models: Models::new(cfg.search.clone()),
            llm: Llm::new(cfg.llm.clone(), db.clone()),
            lex,
            vecs: RwLock::new(Arc::new(vecs)),
            catalog: RwLock::new(Arc::new(catalog)),
            db,
            cfg,
            tasks: TaskQueue::new(),
            refresh_lock: Mutex::new(()),
        })
    }

    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().unwrap().clone()
    }

    pub fn conn(&self) -> Result<rusqlite::Connection> {
        store::open(&self.db)
    }

    /// Imports new session data and updates every index. Lexical search is
    /// current before embedding starts, which can take a while on first run.
    pub fn refresh(&self, rebuild: bool) -> Result<crate::ingest::Stats> {
        self.refresh_with(rebuild, true)
    }

    /// With `embed` false, new turns are searchable by keyword right away and
    /// get vectors on a later pass.
    pub fn refresh_with(&self, rebuild: bool, embed: bool) -> Result<crate::ingest::Stats> {
        let _g = self.refresh_lock.lock().unwrap();
        let mut c = self.conn()?;
        let stats = crate::ingest::Ingester::new(&self.cfg, self.db.with_file_name("archive"))
            .run(&mut c, rebuild)?;
        self.finish_refresh_locked(&mut c, rebuild, embed)?;
        Ok(stats)
    }

    /// Re-syncs and reparses one session from disk, then updates Tantivy,
    /// `Catalog`, and `VecStore` without rescanning the rest of the archive.
    pub fn reindex_session(&self, session_id: &str) -> Result<()> {
        let _g = self.refresh_lock.lock().unwrap();
        let mut c = self.conn()?;
        crate::ingest::Ingester::new(&self.cfg, self.db.with_file_name("archive"))
            .reindex_session(&mut c, session_id)?;
        self.finish_refresh_locked(&mut c, false, false)?;
        Ok(())
    }

    /// Updates Tantivy, `Catalog`, and `VecStore` for sessions already marked
    /// `dirty = 1` in SQLite (for example, right after saving a summary).
    pub fn index_dirty(&self, embed: bool) -> Result<()> {
        let _g = self.refresh_lock.lock().unwrap();
        let mut c = self.conn()?;
        self.finish_refresh_locked(&mut c, false, embed)?;
        Ok(())
    }

    fn finish_refresh_locked(
        &self,
        c: &mut rusqlite::Connection,
        rebuild: bool,
        embed: bool,
    ) -> Result<()> {
        self.lex
            .update(c, self.cfg.search.boilerplate_min_sessions, rebuild)?;
        // Another process can finish indexing and clear dirty flags before this
        // pass. Reload readers even when we did not index anything ourselves.
        self.lex.reload()?;
        *self.catalog.write().unwrap() = Arc::new(Catalog::load(c)?);
        if self.cfg.search.vectors {
            if embed && let Err(e) = crate::models::embed_missing(&self.models, c) {
                tracing::warn!("embedding failed, semantic search is limited: {e:#}");
            }
            *self.vecs.write().unwrap() = Arc::new(VecStore::load(c)?);
        }
        c.execute("UPDATE sessions SET dirty = 0 WHERE dirty = 1", [])?;
        Ok(())
    }

    pub fn parse(&self, q: &str) -> Parsed {
        query::parse(q, &self.catalog(), &self.tickets, &self.cfg.ticket_stoplist)
    }

    pub fn default_options(&self) -> Options {
        Options {
            vectors: self.cfg.search.vectors,
            rerank: self.cfg.search.rerank,
        }
    }

    /// Lexical-only results: the instant first stage.
    pub fn quick(&self, pq: &Parsed) -> Result<Results> {
        self.run(
            pq,
            Options {
                vectors: false,
                rerank: false,
            },
            None,
            "keyword",
        )
    }

    pub fn run(
        &self,
        pq: &Parsed,
        opts: Options,
        rw: Option<&Rewrite>,
        stage: &'static str,
    ) -> Result<Results> {
        let t0 = Instant::now();
        let cat = self.catalog();
        let mut notes = vec![];
        let allowed = |s: &SessionMeta| passes(&cat, s, &pq.filters);

        // Browsing: no text, only filters or entities.
        if pq.text.trim().is_empty() && pq.phrases.is_empty() {
            let mut list: Vec<&SessionMeta> =
                cat.sessions.values().filter(|s| allowed(s)).collect();
            list.sort_by_key(|s| -s.ended);
            let hits = list
                .iter()
                .take(100)
                .map(|s| Hit {
                    session: s.id.clone(),
                    score: 0.0,
                    reasons: vec![],
                    turns: vec![],
                    rerank: None,
                })
                .collect();
            return Ok(Results {
                hits,
                total: list.len(),
                stage,
                took_ms: t0.elapsed().as_millis(),
                ..Default::default()
            });
        }

        let limit = if pq.filters.is_empty() { 600 } else { 4000 };
        let fuzzy = pq.text.split_whitespace().count() <= 3;
        let mut lists: Vec<(f32, &'static str, Vec<(String, Agg)>)> = vec![];

        let lex = self.lex.search(&pq.text, &pq.phrases, limit, fuzzy)?;
        lists.push((1.0, "keyword", aggregate_lex(&lex, &cat, &allowed)));

        let vecs = self.vecs.read().unwrap().clone();
        if opts.vectors && vecs.len() > 0 {
            let t = Instant::now();
            let qv = self.models.embed_query(&pq.text);
            tracing::debug!("embedded query in {} ms", t.elapsed().as_millis());
            match qv {
                Ok(qv) => lists.push((
                    1.0,
                    "semantic",
                    aggregate_vec(&vecs, &vecs.search(&qv, limit), &cat, &allowed),
                )),
                Err(e) => notes.push(format!("semantic search unavailable: {e}")),
            }
        }
        if let Some(rw) = rw.filter(|r| !r.terms.is_empty()) {
            let text = rw.terms.join(" ");
            let lex = self.lex.search(&text, &[], limit, false)?;
            lists.push((0.5, "rewrite", aggregate_lex(&lex, &cat, &allowed)));
            if opts.vectors
                && vecs.len() > 0
                && let Ok(qv) = self.models.embed_query(&format!("{} {}", pq.text, text))
            {
                lists.push((
                    0.5,
                    "rewrite",
                    aggregate_vec(&vecs, &vecs.search(&qv, limit), &cat, &allowed),
                ));
            }
        }

        // Fuse.
        let mut fused: HashMap<String, Hit> = HashMap::new();
        for (w, name, list) in &lists {
            for (rank, (sid, agg)) in list.iter().enumerate() {
                let h = fused.entry(sid.clone()).or_insert_with(|| Hit {
                    session: sid.clone(),
                    score: 0.0,
                    reasons: vec![],
                    turns: vec![],
                    rerank: None,
                });
                h.score += w / (RRF_K + rank as f32 + 1.0);
                if rank < 20 {
                    let reason = if *name == "keyword" && agg.card && agg.turns.is_empty() {
                        "summary"
                    } else {
                        name
                    };
                    if !h.reasons.iter().any(|r| r == reason) {
                        h.reasons.push(reason.to_string());
                    }
                }
                for (t, _) in &agg.turns {
                    if !h.turns.contains(t) && h.turns.len() < 6 {
                        h.turns.push(*t);
                    }
                }
            }
        }
        let unit = 1.0 / RRF_K;
        let now = chrono::Utc::now().timestamp();
        let rw_time = rw.and_then(|r| {
            let s = r
                .since
                .as_deref()
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())?;
            let u = r
                .until
                .as_deref()
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .unwrap_or(chrono::Local::now().date_naive());
            Some((
                s.and_hms_opt(0, 0, 0)?.and_utc().timestamp() - 5 * 86_400,
                u.and_hms_opt(0, 0, 0)?.and_utc().timestamp() + 6 * 86_400,
            ))
        });
        for h in fused.values_mut() {
            let Some(s) = cat.sessions.get(&h.session) else {
                continue;
            };
            if s.kind.as_deref().is_some_and(|k| pq.kinds.contains(&k))
                || rw
                    .and_then(|r| r.kind.as_deref())
                    .is_some_and(|k| s.kind.as_deref() == Some(k))
            {
                h.score += 0.3 * unit;
            }
            if s.outcome
                .as_deref()
                .is_some_and(|o| pq.outcomes.contains(&o))
                || rw
                    .and_then(|r| r.outcome.as_deref())
                    .is_some_and(|o| s.outcome.as_deref() == Some(o))
            {
                h.score += 0.3 * unit;
            }
            let rw_repo = rw
                .and_then(|r| r.repo.as_deref())
                .and_then(|r| query::resolve_repo(&cat, r));
            if s.repo_key.as_ref().is_some_and(|k| {
                pq.repo_boost.contains(k) || rw_repo.as_deref() == Some(k.as_str())
            }) {
                h.score += 0.5 * unit;
                h.reasons.push("repo".into());
            }
            for (from, to) in pq.time_boost.iter().chain(rw_time.iter()) {
                if s.ended >= *from && s.started <= *to {
                    h.score += 0.6 * unit;
                    if !h.reasons.iter().any(|r| r == "time") {
                        h.reasons.push("time".into());
                    }
                }
            }
            // Recency is a gentle tie-breaker, not a filter.
            let age_days = ((now - s.ended).max(0) as f32) / 86_400.0;
            h.score += 0.15 * unit * (-age_days / 90.0).exp();
        }
        // Exact entity hits are pinned above everything else.
        for (sid, branch) in &pq.branch_boost {
            if let Some(h) = fused.get_mut(sid) {
                h.score += 1.5 * unit;
                h.reasons.push(format!("branch {branch}"));
            }
        }
        for e in &pq.exact {
            for sid in &e.sessions {
                let Some(s) = cat.sessions.get(sid) else {
                    continue;
                };
                if !allowed(s) {
                    continue;
                }
                let strength = cat
                    .entities_of(sid, &e.kind)
                    .iter()
                    .filter(|x| e.keys.contains(&x.key.to_lowercase()))
                    .map(|x| x.strength)
                    .max()
                    .unwrap_or(1) as f32;
                let h = fused.entry(sid.clone()).or_insert_with(|| Hit {
                    session: sid.clone(),
                    score: 0.0,
                    reasons: vec![],
                    turns: vec![],
                    rerank: None,
                });
                h.score += 10.0 + strength;
                h.reasons.insert(0, format!("exact {} {}", e.kind, e.label));
            }
        }
        let mut hits: Vec<Hit> = fused.into_values().collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        let total = hits.len();

        let mut confidence = None;
        if opts.rerank && !hits.is_empty() {
            match self.rerank(pq, &cat, &mut hits) {
                Ok(c) => confidence = c,
                Err(e) => notes.push(format!("reranker unavailable: {e}")),
            }
        }
        hits.truncate(100);
        Ok(Results {
            hits,
            total,
            stage,
            took_ms: t0.elapsed().as_millis(),
            notes,
            rewrite: rw.cloned(),
            confidence,
        })
    }

    /// Rescores the top fused sessions with the cross-encoder. Pinned exact
    /// hits stay on top; the rest are ordered by rerank score, with the fused
    /// score as a small tie-breaker.
    fn rerank(&self, pq: &Parsed, cat: &Catalog, hits: &mut Vec<Hit>) -> Result<Option<f32>> {
        let top = self.cfg.search.rerank_top.min(hits.len());
        let c = self.conn()?;
        let ids: Vec<i64> = hits[..top]
            .iter()
            .flat_map(|h| h.turns.iter().take(2).copied())
            .collect();
        let texts = store::turn_texts(&c, &ids)?;
        let docs: Vec<String> = hits[..top]
            .iter()
            .map(|h| {
                let s = &cat.sessions[&h.session];
                let mut d = format!(
                    "{}\n{}\n",
                    s.title,
                    s.summary.as_deref().or(s.recap.as_deref()).unwrap_or("")
                );
                for t in h.turns.iter().take(2) {
                    if let Some((_, u, a)) = texts.get(t) {
                        d.push_str(&crate::model::clip_chars(u, 350));
                        d.push('\n');
                        d.push_str(&crate::model::clip_chars(a, 350));
                        d.push('\n');
                    }
                }
                crate::model::clip_chars(&d, 1600)
            })
            .collect();
        let q = if pq.text.is_empty() {
            pq.raw.clone()
        } else {
            pq.text.clone()
        };
        let t = Instant::now();
        let scores = self.models.rerank(&q, &docs)?;
        tracing::debug!(
            "reranked {} sessions in {} ms",
            docs.len(),
            t.elapsed().as_millis()
        );
        let max_fused = hits[..top]
            .iter()
            .filter(|h| h.score < 10.0)
            .map(|h| h.score)
            .fold(0.0f32, f32::max)
            .max(1e-6);
        for (h, s) in hits[..top].iter_mut().zip(&scores) {
            let p = 1.0 / (1.0 + (-s).exp());
            h.rerank = Some(p);
            let pinned = if h.score >= 10.0 { h.score } else { 0.0 };
            h.score = pinned + p + 0.25 * (h.score.min(max_fused) / max_fused);
        }
        let mut head: Vec<Hit> = hits.drain(..top).collect();
        head.sort_by(|a, b| b.score.total_cmp(&a.score));
        let confidence = head
            .iter()
            .filter_map(|h| h.rerank)
            .fold(None, |m: Option<f32>, p| Some(m.map_or(p, |m| m.max(p))));
        // Unreranked sessions keep their fused order below the reranked ones.
        for h in hits.iter_mut() {
            h.score = h.score.min(1.0) * 0.01;
        }
        head.append(hits);
        *hits = head;
        Ok(confidence)
    }

    /// Whether an LLM rewrite is likely to help this query.
    pub fn wants_rewrite(&self, pq: &Parsed, res: &Results) -> bool {
        self.llm.rewrite_enabled() && pq.sentence && res.confidence.is_none_or(|c| c < 0.6)
    }

    pub async fn rewrite(&self, pq: &Parsed) -> Result<Rewrite> {
        let cat = self.catalog();
        let mut repos: Vec<String> = cat.repos.values().cloned().collect();
        repos.sort();
        repos.dedup();
        repos.truncate(80);
        self.llm.rewrite(&pq.raw, &repos).await
    }
}

fn passes(cat: &Catalog, s: &SessionMeta, f: &query::Filters) -> bool {
    if let Some(r) = &f.repo {
        let ok = s.repo_key.as_deref() == Some(r.as_str())
            || cat.repo_name(s).is_some_and(|n| n.eq_ignore_ascii_case(r));
        if !ok {
            return false;
        }
    }
    if f.agent.as_ref().is_some_and(|a| *a != s.source) {
        return false;
    }
    if let Some(b) = &f.branch
        && !cat.entities_of(&s.id, "branch").iter().any(|e| e.key == *b)
    {
        return false;
    }
    if f.kind.as_ref().is_some_and(|k| s.kind.as_ref() != Some(k))
        || f.outcome
            .as_ref()
            .is_some_and(|o| s.outcome.as_ref() != Some(o))
    {
        return false;
    }
    if f.since.is_some_and(|t| s.ended < t) || f.until.is_some_and(|t| s.started > t) {
        return false;
    }
    true
}

fn finish(map: HashMap<String, Agg>) -> Vec<(String, Agg)> {
    let mut v: Vec<(String, Agg)> = map.into_iter().collect();
    for (_, a) in v.iter_mut() {
        a.turns.sort_by(|x, y| y.1.total_cmp(&x.1));
    }
    // Best document, plus a small bonus for other matching documents.
    v.sort_by(|a, b| (b.1.best + 0.15 * b.1.rest).total_cmp(&(a.1.best + 0.15 * a.1.rest)));
    v
}

fn add(map: &mut HashMap<String, Agg>, sid: &str, turn: Option<i64>, score: f32) {
    let a = map.entry(sid.to_string()).or_default();
    if score > a.best {
        a.rest += a.best;
        a.best = score;
    } else if a.n < 5 {
        a.rest += score;
    }
    a.n += 1;
    match turn {
        Some(t) => a.turns.push((t, score)),
        None => a.card = true,
    }
}

fn aggregate_lex(
    hits: &[LexHit],
    cat: &Catalog,
    allowed: &dyn Fn(&SessionMeta) -> bool,
) -> Vec<(String, Agg)> {
    let mut map = HashMap::new();
    for h in hits {
        if cat.sessions.get(&h.session).is_some_and(allowed) {
            add(&mut map, &h.session, h.turn, h.score);
        }
    }
    finish(map)
}

fn aggregate_vec(
    vs: &VecStore,
    hits: &[(usize, f32)],
    cat: &Catalog,
    allowed: &dyn Fn(&SessionMeta) -> bool,
) -> Vec<(String, Agg)> {
    let mut map = HashMap::new();
    for (i, score) in hits {
        let (sid, turn) = &vs.docs[*i];
        if cat.sessions.get(sid).is_some_and(allowed) {
            add(&mut map, sid, *turn, *score);
        }
    }
    finish(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_picks_up_summaries_indexed_by_another_process() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = store::open(&dir.path().join("recherche.db"))?;
        c.execute(
            "INSERT INTO sessions (id, source, host, title, summary, started, ended, turn_count, archive_path)
             VALUES ('session', 'claude', 'test', 'Original title', 'Original summary', 1, 2, 1, '')",
            [],
        )?;
        let mut cfg = Config::default();
        cfg.sources.claude.dir = Some(dir.path().join("claude"));
        cfg.sources.codex.dir = Some(dir.path().join("codex"));
        cfg.sources.antigravity.dir = Some(dir.path().join("antigravity"));
        cfg.llm.enabled = false;
        cfg.search.rerank = false;
        let cli = Engine::open_in(cfg.clone(), dir.path())?;
        cli.refresh_with(false, false)?;
        c.execute(
            "INSERT INTO vecs (hash, v) SELECT card_hash, ? FROM sessions WHERE id = 'session'",
            [1.0_f32.to_le_bytes().as_slice()],
        )?;
        let server = Engine::open_in(cfg, dir.path())?;
        assert_eq!(server.vecs.read().unwrap().data, [1.0]);

        // The CLI writes a summary and finishes indexing before the server polls.
        c.execute(
            "UPDATE sessions SET summary = 'Quasar analysis', llm_title = 'Updated title',
             summary_turns = 1, dirty = 1 WHERE id = 'session'",
            [],
        )?;
        cli.refresh_with(false, false)?;
        c.execute(
            "INSERT INTO vecs (hash, v) SELECT card_hash, ? FROM sessions WHERE id = 'session'",
            [2.0_f32.to_le_bytes().as_slice()],
        )?;
        let dirty: i64 = c.query_row("SELECT SUM(dirty) FROM sessions", [], |r| r.get(0))?;
        assert_eq!(dirty, 0);
        assert_eq!(server.catalog().sessions["session"].title, "Original title");
        assert!(server.quick(&server.parse("quasar"))?.hits.is_empty());
        assert_eq!(server.vecs.read().unwrap().data, [1.0]);

        server.refresh_with(false, false)?;
        let cat = server.catalog();
        assert_eq!(cat.sessions["session"].title, "Updated title");
        assert_eq!(
            cat.sessions["session"].summary.as_deref(),
            Some("Quasar analysis")
        );
        let results = server.quick(&server.parse("quasar"))?;
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].session, "session");
        assert_eq!(server.vecs.read().unwrap().data, [2.0]);
        Ok(())
    }

    #[test]
    fn reindex_session_refreshes_catalog_and_lexical_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("claude/projects/repo");
        std::fs::create_dir_all(&project)?;
        let line = |text: &str| {
            serde_json::json!({
                "type": "user", "sessionId": "s1", "cwd": "/repo",
                "timestamp": "2026-01-01T00:00:00Z", "message": {"role": "user", "content": text},
            })
            .to_string()
        };
        let path = project.join("s1.jsonl");
        std::fs::write(&path, format!("{}\n", line("initial turn")))?;

        let mut cfg = Config::default();
        cfg.sources.claude.dir = Some(dir.path().join("claude"));
        cfg.sources.codex.dir = Some(dir.path().join("codex"));
        cfg.sources.antigravity.dir = Some(dir.path().join("antigravity"));
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        cfg.llm.enabled = false;
        let engine = Engine::open_in(cfg, &dir.path().join("data"))?;
        engine.refresh_with(false, false)?;
        assert_eq!(engine.catalog().sessions["s1"].turn_count, 1);
        assert!(engine.quick(&engine.parse("quasar"))?.hits.is_empty());

        std::fs::write(
            &path,
            format!("{}\n{}\n", line("initial turn"), line("quasar follow-up")),
        )?;
        engine.reindex_session("s1")?;
        assert_eq!(engine.catalog().sessions["s1"].turn_count, 2);
        let hits = engine.quick(&engine.parse("quasar"))?.hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session, "s1");
        Ok(())
    }
}
