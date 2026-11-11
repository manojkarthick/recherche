//! `recherche eval`: scores ranking configurations against real queries.
//!
//! Cases come from a TOML file and from the "This was it" buttons in the
//! web UI. A ranking change should improve at least one class and make none
//! worse.
//!
//! ```toml
//! [[query]]
//! q = "the thread where we fixed the ordering issue"
//! class = "conversational"   # or exact, entity, temporal
//! expect = ["<session id>"]
//! ```

use crate::search::{Engine, Options};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize, Default)]
struct File {
    #[serde(default)]
    query: Vec<Case>,
}

#[derive(Deserialize, Clone)]
struct Case {
    q: String,
    #[serde(default = "default_class")]
    class: String,
    expect: Vec<String>,
}

fn default_class() -> String {
    "conversational".into()
}

fn load(engine: &Engine, path: &Path) -> Result<Vec<Case>> {
    let mut cases = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        toml::from_str::<File>(&text).with_context(|| format!("parsing {}", path.display()))?.query
    } else {
        vec![]
    };
    let c = engine.conn()?;
    let mut st = c.prepare("SELECT query, session_id FROM feedback WHERE label = 'good' ORDER BY ts")?;
    let mut fb: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (q, s) = row?;
        let v = fb.entry(q).or_default();
        if !v.contains(&s) {
            v.push(s);
        }
    }
    for (q, expect) in fb {
        if !cases.iter().any(|c| c.q == q) {
            cases.push(Case { q, class: "feedback".into(), expect });
        }
    }
    Ok(cases)
}

#[derive(Default, Clone, Copy)]
struct Score {
    n: usize,
    r5: usize,
    r10: usize,
    rr: f64,
}

pub fn run(engine: &Engine, path: &Path, with_rewrite: bool, verbose: bool) -> Result<()> {
    let cases = load(engine, path)?;
    if cases.is_empty() {
        println!("No eval cases yet. Add queries to {} or press \"This was it\" on results in the web UI.", path.display());
        return Ok(());
    }
    let mut configs: Vec<(&str, Options, bool)> = vec![
        ("keyword", Options { vectors: false, rerank: false }, false),
        ("+vectors", Options { vectors: true, rerank: false }, false),
        ("+rerank", Options { vectors: true, rerank: true }, false),
    ];
    if with_rewrite {
        configs.push(("+rewrite", Options { vectors: true, rerank: true }, true));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut table: Vec<(&str, BTreeMap<String, Score>)> = vec![];
    for (name, opts, rewrite) in &configs {
        let mut by_class: BTreeMap<String, Score> = BTreeMap::new();
        for case in &cases {
            let pq = engine.parse(&case.q);
            let mut res = engine.run(&pq, *opts, None, "eval")?;
            if *rewrite && engine.wants_rewrite(&pq, &res)
                && let Ok(rw) = rt.block_on(engine.rewrite(&pq)) {
                    res = engine.run(&pq, *opts, Some(&rw), "eval")?;
                }
            let rank = res.hits.iter().position(|h| case.expect.iter().any(|x| h.session == *x || h.session.starts_with(&format!("{x}/"))));
            for class in [case.class.clone(), "all".into()] {
                let s = by_class.entry(class).or_default();
                s.n += 1;
                if let Some(r) = rank {
                    s.r5 += (r < 5) as usize;
                    s.r10 += (r < 10) as usize;
                    s.rr += 1.0 / (r as f64 + 1.0);
                }
            }
            if verbose {
                println!("{name:<9} {:>4}  {}", rank.map(|r| (r + 1).to_string()).unwrap_or("miss".into()), case.q);
            }
        }
        table.push((name, by_class));
    }
    println!("{:<9} {:<15} {:>4} {:>8} {:>8} {:>6}", "config", "class", "n", "recall@5", "recall@10", "MRR");
    for (name, classes) in &table {
        for (class, s) in classes {
            let pct = |x: usize| 100.0 * x as f64 / s.n.max(1) as f64;
            println!("{name:<9} {class:<15} {:>4} {:>7.0}% {:>8.0}% {:>6.3}", s.n, pct(s.r5), pct(s.r10), s.rr / s.n.max(1) as f64);
        }
    }
    Ok(())
}
