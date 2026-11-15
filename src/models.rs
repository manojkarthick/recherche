//! Local non-LLM models: a bge-small embedder and a cross-encoder reranker,
//! both ONNX through `ort` inside this process. They load on first use and
//! unload after `model_idle_seconds`, so the idle daemon stays small.

use crate::config::{Search, cache_dir};
use crate::util::count;
use anyhow::{Result, anyhow};
use fastembed::{
    AutoDevicePolicy, EmbeddingModel, RerankInitOptions, RerankerModel, TextEmbedding,
    TextInitOptions, TextRerank,
};
use rusqlite::{Connection, params};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Half the cores, so indexing never takes over the machine.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1))
}

/// bge models expect this prefix on queries, not on documents.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// A loaded model and when a search last used it. Background indexing does
/// not count as use, so the daemon drops a model it loaded only for indexing
/// as soon as that work is done.
type Slot<M> = Mutex<Option<(M, Option<Instant>)>>;

pub struct Models {
    cfg: Search,
    embed: Slot<TextEmbedding>,
    rerank: Slot<TextRerank>,
}

impl Models {
    pub fn new(cfg: Search) -> Self {
        Self {
            cfg,
            embed: Mutex::new(None),
            rerank: Mutex::new(None),
        }
    }

    fn with_embedder<T>(
        &self,
        query: bool,
        f: impl FnOnce(&mut TextEmbedding) -> Result<T>,
    ) -> Result<T> {
        let mut g = self.embed.lock().unwrap();
        if g.is_none() {
            let m = TextEmbedding::try_new(
                // The default auto-device policy picks CoreML on macOS, which recompiles the
                // model for every new input shape: many GB of memory and minutes per batch.
                TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                    .with_cache_dir(cache_dir().join("models"))
                    .with_show_download_progress(false)
                    .with_max_length(256)
                    .with_auto_device(AutoDevicePolicy::PreferCPU)
                    .with_intra_threads(threads()),
            )
            .map_err(|e| anyhow!("loading embedding model: {e}"))?;
            *g = Some((m, None));
        }
        let (m, used) = g.as_mut().unwrap();
        if query {
            *used = Some(Instant::now());
        }
        f(m)
    }

    pub fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.with_embedder(false, |m| {
            m.embed(texts, Some(32)).map_err(|e| anyhow!("{e}"))
        })
    }

    pub fn embed_query(&self, q: &str) -> Result<Vec<f32>> {
        let mut v = self.with_embedder(true, |m| {
            m.embed(&[format!("{QUERY_PREFIX}{q}")], None)
                .map_err(|e| anyhow!("{e}"))
        })?;
        Ok(normalize(v.remove(0)))
    }

    /// Relevance logits for each document, in input order.
    pub fn rerank(&self, q: &str, docs: &[String]) -> Result<Vec<f32>> {
        let mut g = self.rerank.lock().unwrap();
        if g.is_none() {
            let model = if self.cfg.rerank_model == "bge-base" {
                RerankerModel::BGERerankerBase
            } else {
                RerankerModel::JINARerankerV1TurboEn
            };
            let m = TextRerank::try_new(
                RerankInitOptions::new(model)
                    .with_cache_dir(cache_dir().join("models"))
                    .with_show_download_progress(false)
                    .with_max_length(256)
                    .with_auto_device(AutoDevicePolicy::PreferCPU)
                    .with_intra_threads(
                        std::thread::available_parallelism().map_or(4, |n| n.get()),
                    ),
            )
            .map_err(|e| anyhow!("loading reranker: {e}"))?;
            *g = Some((m, None));
        }
        let (m, used) = g.as_mut().unwrap();
        *used = Some(Instant::now());
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let res = m
            .rerank(q, refs, false, Some(64))
            .map_err(|e| anyhow!("{e}"))?;
        let mut scores = vec![f32::MIN; docs.len()];
        for r in res {
            scores[r.index] = r.score;
        }
        Ok(scores)
    }

    /// Drops models no search used recently. Called by the daemon after
    /// every index pass.
    pub fn unload_idle(&self) {
        let idle = Duration::from_secs(self.cfg.model_idle_seconds);
        let stale = |t: &Option<Instant>| t.is_none_or(|t| t.elapsed() > idle);
        let mut g = self.embed.lock().unwrap();
        if g.as_ref().is_some_and(|(_, t)| stale(t)) {
            *g = None;
            tracing::debug!("unloaded embedding model");
        }
        let mut g = self.rerank.lock().unwrap();
        if g.as_ref().is_some_and(|(_, t)| stale(t)) {
            *g = None;
            tracing::debug!("unloaded reranker");
        }
    }
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// The text a turn is embedded from: the prompt carries most of the meaning.
fn turn_embed_text(user: &str, assistant: &str) -> String {
    format!(
        "{}\n{}",
        crate::model::clip_chars(user, 900),
        crate::model::clip_chars(assistant, 600)
    )
}

/// Embeds turns and cards that have no vector yet. Vectors are keyed by a
/// content hash, so a growing session only embeds its new turns.
pub fn embed_missing(models: &Models, c: &Connection) -> Result<usize> {
    let started = Instant::now();
    let mut todo: Vec<(String, String)> = vec![];
    let turns;
    {
        let mut st = c.prepare(
            "SELECT DISTINCT t.vhash, t.clean_user, t.assistant_text FROM turns t JOIN sessions s ON s.id = t.session_id
             WHERE s.skip = 0 AND t.dup = 0 AND t.vhash IS NOT NULL AND (t.clean_user != '' OR t.assistant_text != '')
               AND NOT EXISTS (SELECT 1 FROM vecs v WHERE v.hash = t.vhash)",
        )?;
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (h, u, a) = row?;
            todo.push((h, turn_embed_text(&u, &a)));
        }
        let mut st = c.prepare(
            "SELECT card_hash, card_text FROM sessions s WHERE skip = 0 AND card_hash IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM vecs v WHERE v.hash = s.card_hash)",
        )?;
        turns = todo.len();
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (h, t) = row?;
            todo.push((h, crate::model::clip_chars(&t, 1500)));
        }
    }
    let mut done = 0;
    for chunk in todo.chunks(128) {
        let texts: Vec<String> = chunk.iter().map(|x| x.1.clone()).collect();
        let vecs = models.embed_docs(&texts)?;
        let mut ins = c.prepare_cached("INSERT OR REPLACE INTO vecs (hash, v) VALUES (?, ?)")?;
        for ((h, _), v) in chunk.iter().zip(vecs) {
            let v = normalize(v);
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            ins.execute(params![h, bytes])?;
        }
        done += chunk.len();
        // Progress matters only for large backlogs, like the first run.
        if todo.len() > 128 {
            tracing::info!("embedded {done}/{}", todo.len());
        }
    }
    if done > 0 {
        let cards = done - turns;
        let what = match (turns, cards) {
            (_, 0) => count(turns, "turn"),
            (0, _) => count(cards, "session card"),
            _ => format!(
                "{} and {}",
                count(turns, "turn"),
                count(cards, "session card")
            ),
        };
        tracing::info!("embedded {what} in {:.1}s", started.elapsed().as_secs_f32());
    }
    Ok(done)
}

/// All document vectors in memory: about 1.5 KB each. A brute-force scan of
/// tens of thousands takes a few milliseconds.
#[derive(Default)]
pub struct VecStore {
    pub dim: usize,
    pub data: Vec<f32>,
    /// (session, turn id or None for the card)
    pub docs: Vec<(String, Option<i64>)>,
}

impl VecStore {
    pub fn load(c: &Connection) -> Result<Self> {
        let mut vs = VecStore::default();
        let mut push = |sid: String, turn: Option<i64>, blob: Vec<u8>| {
            let v: Vec<f32> = blob
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            if vs.dim == 0 {
                vs.dim = v.len();
            }
            if v.len() == vs.dim {
                vs.data.extend_from_slice(&v);
                vs.docs.push((sid, turn));
            }
        };
        let mut st = c.prepare(
            "SELECT t.session_id, t.id, v.v FROM turns t JOIN sessions s ON s.id = t.session_id JOIN vecs v ON v.hash = t.vhash
             WHERE s.skip = 0 AND t.dup = 0",
        )?;
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })? {
            let (s, t, b) = row?;
            push(s, Some(t), b);
        }
        let mut st = c.prepare(
            "SELECT s.id, v.v FROM sessions s JOIN vecs v ON v.hash = s.card_hash WHERE s.skip = 0",
        )?;
        for row in st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })? {
            let (s, b) = row?;
            push(s, None, b);
        }
        Ok(vs)
    }

    /// Top `k` documents by cosine similarity.
    pub fn search(&self, q: &[f32], k: usize) -> Vec<(usize, f32)> {
        if self.dim == 0 || q.len() != self.dim {
            return vec![];
        }
        let mut scored: Vec<(usize, f32)> = self
            .data
            .chunks_exact(self.dim)
            .enumerate()
            .map(|(i, v)| (i, v.iter().zip(q).map(|(a, b)| a * b).sum()))
            .collect();
        let k = k.min(scored.len());
        if k == 0 {
            return vec![];
        }
        scored.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }
}
