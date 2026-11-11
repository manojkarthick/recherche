//! Tantivy index of turn documents and session cards.
//!
//! Prose fields use an English stemmer with stop words removed. The `ident`
//! field holds identifiers, paths, commands and errors, split on `_ - . /` and
//! camelCase with the whole token kept, so both `history_table` and `history`
//! match.

use crate::util::expand_idents;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer, StopWordFilter, TextAnalyzer, WhitespaceTokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

#[derive(Clone, Copy)]
struct Fields {
    session: Field,
    turn: Field,
    user: Field,
    assistant: Field,
    tool: Field,
    ident: Field,
    card: Field,
}

pub struct Lexical {
    index: Index,
    reader: IndexReader,
    f: Fields,
}

#[derive(Debug, Clone)]
pub struct LexHit {
    pub session: String,
    /// None for a session card.
    pub turn: Option<i64>,
    pub score: f32,
}

/// Field weights: the user's own words count most, tool output least.
const BOOSTS: [(&str, f32); 5] = [("user", 3.0), ("card", 2.5), ("assistant", 1.2), ("ident", 1.0), ("tool", 0.4)];

fn schema() -> Schema {
    let mut b = Schema::builder();
    let en = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("en").set_index_option(IndexRecordOption::WithFreqsAndPositions));
    let ident = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("ident").set_index_option(IndexRecordOption::WithFreqs));
    b.add_text_field("session", STRING | STORED);
    b.add_i64_field("turn", INDEXED | STORED | FAST);
    b.add_text_field("user", en.clone());
    b.add_text_field("assistant", en.clone());
    b.add_text_field("tool", en.clone());
    b.add_text_field("card", en);
    b.add_text_field("ident", ident);
    b.build()
}

impl Lexical {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let index = match Index::open_in_dir(dir) {
            Ok(i) => i,
            Err(_) => Index::create_in_dir(dir, schema())?,
        };
        index.tokenizers().register(
            "en",
            TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(StopWordFilter::new(Language::English).unwrap())
                .filter(Stemmer::new(Language::English))
                .build(),
        );
        index
            .tokenizers()
            .register("ident", TextAnalyzer::builder(WhitespaceTokenizer::default()).filter(RemoveLongFilter::limit(120)).filter(LowerCaser).build());
        let s = index.schema();
        let g = |n: &str| s.get_field(n).unwrap();
        let f = Fields {
            session: g("session"),
            turn: g("turn"),
            user: g("user"),
            assistant: g("assistant"),
            tool: g("tool"),
            ident: g("ident"),
            card: g("card"),
        };
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        Ok(Self { index, reader, f })
    }

    pub fn writer(&self) -> Result<IndexWriter> {
        Ok(self.index.writer(50_000_000)?)
    }

    pub fn reload(&self) -> Result<()> {
        Ok(self.reader.reload()?)
    }

    /// Rewrites the documents of every dirty session. Also writes the cleaned
    /// prompt, duplicate flag and embedding key of each turn, and the card.
    pub fn update(&self, c: &mut Connection, boilerplate_min: i64, everything: bool) -> Result<usize> {
        let boiler: HashSet<String> = {
            let mut st = c.prepare("SELECT hash FROM blocks GROUP BY hash HAVING COUNT(*) >= ?")?;
            st.query_map([boilerplate_min], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let dirty: Vec<(String, bool)> = {
            let sql = if everything { "SELECT id, skip FROM sessions" } else { "SELECT id, skip FROM sessions WHERE dirty = 1" };
            let mut st = c.prepare(sql)?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        if dirty.is_empty() {
            return Ok(0);
        }
        let mut w = self.writer()?;
        if everything {
            w.delete_all_documents()?;
        }
        let tx = c.transaction()?;
        for (sid, skip) in &dirty {
            w.delete_term(Term::from_field_text(self.f.session, sid));
            if *skip {
                continue;
            }
            let rows: Vec<(i64, String, String, String, String, String, String, String)> = {
                let mut st = tx.prepare_cached(
                    "SELECT id, user_text, assistant_text, tool_text, files, commands, errors, ukey FROM turns WHERE session_id = ? ORDER BY idx",
                )?;
                st.query_map([sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
                    .collect::<Result<_, _>>()?
            };
            for (id, user, assistant, tool, files, commands, errors, ukey) in rows {
                let owner: String = tx
                    .prepare_cached(
                        "SELECT t.session_id FROM turns t JOIN sessions s ON s.id = t.session_id
                         WHERE t.ukey = ? AND s.skip = 0 ORDER BY s.started, s.id LIMIT 1",
                    )?
                    .query_row([&ukey], |r| r.get(0))?;
                let dup = owner != *sid;
                let clean = clean_prompt(&user, &boiler);
                let vhash = crate::util::hash(&[&clean, &assistant]);
                tx.prepare_cached("UPDATE turns SET clean_user = ?, dup = ?, vhash = ? WHERE id = ?")?.execute(params![clean, dup, vhash, id])?;
                if dup || (clean.is_empty() && assistant.is_empty()) {
                    continue;
                }
                let mut d = TantivyDocument::default();
                d.add_text(self.f.session, sid);
                d.add_i64(self.f.turn, id);
                d.add_text(self.f.user, &clean);
                d.add_text(self.f.assistant, &assistant);
                d.add_text(self.f.tool, &tool);
                d.add_text(self.f.ident, expand_idents(&format!("{clean}\n{assistant}\n{files}\n{commands}\n{errors}")));
                w.add_document(d)?;
            }
            let card = card_text(&tx, sid)?;
            let card_hash = crate::util::hash(&["card", &card]);
            tx.execute("UPDATE sessions SET card_text = ?, card_hash = ? WHERE id = ?", params![card, card_hash, sid])?;
            let mut d = TantivyDocument::default();
            d.add_text(self.f.session, sid);
            d.add_i64(self.f.turn, -1);
            d.add_text(self.f.card, &card);
            d.add_text(self.f.ident, expand_idents(&card));
            w.add_document(d)?;
        }
        w.commit()?;
        tx.commit()?;
        self.reload()?;
        Ok(dirty.len())
    }

    /// BM25 over all fields with their boosts. `phrases` must appear in order.
    pub fn search(&self, text: &str, phrases: &[String], limit: usize, fuzzy: bool) -> Result<Vec<LexHit>> {
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![];
        for (name, boost) in BOOSTS {
            let field = self.field(name);
            let input = if name == "ident" { expand_idents(text) } else { text.to_string() };
            let terms = self.tokens(field, &input);
            if terms.is_empty() {
                continue;
            }
            let q: Vec<(Occur, Box<dyn Query>)> = terms
                .iter()
                .map(|t| (Occur::Should, Box::new(TermQuery::new(Term::from_field_text(field, t), IndexRecordOption::WithFreqs)) as Box<dyn Query>))
                .collect();
            clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(BooleanQuery::new(q)), boost))));
            if fuzzy && name == "ident" {
                for t in terms.iter().filter(|t| t.chars().count() >= 5) {
                    let fq = FuzzyTermQuery::new(Term::from_field_text(field, t), 1, true);
                    clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(fq), 0.3))));
                }
            }
        }
        for p in phrases {
            for name in ["user", "assistant", "card", "tool"] {
                let field = self.field(name);
                let terms: Vec<Term> = self.tokens(field, p).into_iter().map(|t| Term::from_field_text(field, &t)).collect();
                match terms.len() {
                    0 => {}
                    1 => clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(terms[0].clone(), IndexRecordOption::WithFreqs)), 4.0)))),
                    _ => clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(PhraseQuery::new(terms)), 4.0)))),
                }
            }
        }
        if clauses.is_empty() {
            return Ok(vec![]);
        }
        let q = BooleanQuery::new(clauses);
        let top = searcher.search(&q, &TopDocs::with_limit(limit).order_by_score())?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            let session = d.get_first(self.f.session).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let turn = d.get_first(self.f.turn).and_then(|v| v.as_i64()).filter(|t| *t >= 0);
            out.push(LexHit { session, turn, score });
        }
        Ok(out)
    }

    fn tokens(&self, field: Field, text: &str) -> Vec<String> {
        let Ok(mut a) = self.index.tokenizer_for_field(field) else { return vec![] };
        let mut ts = a.token_stream(text);
        let mut out = vec![];
        while ts.advance() {
            let t = ts.token().text.clone();
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }

    fn field(&self, name: &str) -> Field {
        match name {
            "user" => self.f.user,
            "assistant" => self.f.assistant,
            "tool" => self.f.tool,
            "card" => self.f.card,
            _ => self.f.ident,
        }
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
}

fn clean_prompt(user: &str, boiler: &HashSet<String>) -> String {
    if boiler.is_empty() {
        return user.to_string();
    }
    user.split("\n\n")
        .filter(|p| {
            let norm = p.split_whitespace().collect::<Vec<_>>().join(" ");
            norm.len() < 80 || !boiler.contains(&crate::util::hash(&[&norm]))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The session card: what the session was about, in one document. Vague
/// queries often match this better than any single turn.
pub fn card_text(c: &Connection, sid: &str) -> Result<String> {
    let (title, recap, summary, llm_title, kind, outcome, tags, repo): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = c.query_row(
        "SELECT title, recap, summary, llm_title, kind, outcome, tags, repo_key FROM sessions WHERE id = ?",
        [sid],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
    )?;
    let mut parts: Vec<String> = [title, llm_title, summary, recap, kind, outcome, tags].into_iter().flatten().collect();
    if let Some(r) = repo {
        parts.push(crate::extract::repo_display(&r));
    }
    let mut st = c.prepare_cached(
        "SELECT kind, display FROM entities WHERE session_id = ? AND kind IN ('pr', 'branch', 'ticket', 'commit', 'file') ORDER BY kind, strength DESC",
    )?;
    let mut files = 0;
    for row in st.query_map([sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (k, d) = row?;
        if k == "file" {
            files += 1;
            if files > 20 {
                continue;
            }
        }
        parts.push(d);
    }
    Ok(parts.join("\n"))
}
