//! The web UI: server-rendered askama templates, made interactive with htmx.
//! Results stream in stages over SSE: keyword results first, then hybrid
//! and reranked, then (when it helps) results refined by an LLM rewrite.

use crate::search::{Engine, Hit, Results};
use crate::store::{Catalog, SessionMeta};
use crate::util::{fmt_ago, fmt_date};
use anyhow::Result;
use askama::Template;
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

type AppState = Arc<Engine>;

pub async fn serve(engine: Arc<Engine>, listen: &str) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/search", get(search_fragment))
        .route("/stream", get(stream))
        .route("/s/{*id}", get(session_page))
        .route("/e", get(entity_page))
        .route("/feedback", post(feedback))
        .route("/assets/{*path}", get(asset))
        .layer(middleware::from_fn_with_state(engine.clone(), auth))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("listening on http://{listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Off loopback, every request needs the configured token: once as
/// `?token=`, which sets a cookie, or in that cookie afterwards.
async fn auth(State(e): State<AppState>, req: Request, next: Next) -> Response {
    let Some(token) = e.cfg.token.clone().filter(|_| !e.cfg.is_loopback()) else {
        return next.run(req).await;
    };
    let cookie_ok = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| {
            c.split(';')
                .any(|kv| kv.trim() == format!("recherche_token={token}"))
        });
    let query_ok = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|kv| kv == format!("token={token}")));
    if !cookie_ok && !query_ok {
        return (
            StatusCode::UNAUTHORIZED,
            "Open this page with ?token=<your token> once.",
        )
            .into_response();
    }
    let mut resp = next.run(req).await;
    if query_ok
        && !cookie_ok
        && let Ok(v) = HeaderValue::from_str(&format!(
            "recherche_token={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000"
        ))
    {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

async fn asset(AxPath(path): AxPath<String>) -> Response {
    match Assets::get(&path) {
        Some(f) => {
            let mime = match path.rsplit('.').next() {
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                f.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Params {
    #[serde(default)]
    q: String,
    repo: Option<String>,
    agent: Option<String>,
    branch: Option<String>,
    kind: Option<String>,
    outcome: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

impl Params {
    fn filters(&self) -> Vec<(&'static str, &str)> {
        [
            ("repo", &self.repo),
            ("agent", &self.agent),
            ("branch", &self.branch),
            ("kind", &self.kind),
            ("outcome", &self.outcome),
            ("since", &self.since),
            ("until", &self.until),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.as_deref().filter(|v| !v.is_empty()).map(|v| (k, v)))
        .collect()
    }

    /// The query with filters as operators, the form the parser reads.
    fn full_query(&self) -> String {
        let mut q = self.q.clone();
        for (k, v) in self.filters() {
            if v.contains(' ') {
                q.push_str(&format!(" {k}:\"{v}\""));
            } else {
                q.push_str(&format!(" {k}:{v}"));
            }
        }
        q
    }

    fn url(&self, path: &str, toggle: Option<(&str, &str)>) -> String {
        let mut pairs: Vec<(String, String)> = vec![];
        if !self.q.is_empty() {
            pairs.push(("q".into(), self.q.clone()));
        }
        for (k, v) in self.filters() {
            if toggle.is_some_and(|(tk, _)| tk == k) {
                continue;
            }
            pairs.push((k.into(), v.into()));
        }
        if let Some((k, v)) = toggle {
            let active = self.filters().iter().any(|(fk, fv)| *fk == k && *fv == v);
            if !active {
                pairs.push((k.into(), v.into()));
            }
        }
        let qs: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", enc(v)))
            .collect();
        if qs.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", qs.join("&"))
        }
    }

    fn is_empty(&self) -> bool {
        self.q.trim().is_empty() && self.filters().is_empty()
    }
}

pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---- view models ----

pub struct Chip {
    pub label: String,
    pub href: String,
    pub class: &'static str,
    pub title: String,
}

pub struct ResultView {
    pub href: String,
    pub title: String,
    pub agent: String,
    pub is_sub: bool,
    pub chips: Vec<Chip>,
    pub ago: String,
    pub when: String,
    pub blurb: Option<String>,
    pub snippets: Vec<String>,
    pub reasons: Vec<String>,
    pub resume: String,
    pub cwd_gone: bool,
    pub session_id: String,
}

pub struct FacetItem {
    pub label: String,
    pub count: usize,
    pub href: String,
    pub active: bool,
}

pub struct FacetGroup {
    pub name: &'static str,
    pub items: Vec<FacetItem>,
}

#[derive(Template)]
#[template(path = "results.html")]
struct ResultsTpl {
    results: Vec<ResultView>,
    facets: Vec<FacetGroup>,
    status: String,
    refining: bool,
    notes: Vec<String>,
    stream_url: Option<String>,
    q: String,
    empty_hint: Option<String>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl {
    q: String,
    hidden: Vec<(String, String)>,
    active: Vec<Chip>,
    results_html: String,
    sessions: usize,
}

pub fn chips_for(cat: &Catalog, s: &SessionMeta) -> Vec<Chip> {
    let mut chips = vec![];
    if let Some(k) = &s.repo_key {
        let inferred = s.repo_origin.as_deref() == Some("inferred");
        chips.push(Chip {
            label: cat.repo_name(s).unwrap_or_default(),
            href: format!("/e?kind=repo&key={}", enc(k)),
            class: if inferred {
                "chip repo inferred"
            } else {
                "chip repo"
            },
            title: if inferred {
                "Repo inferred from the directory".into()
            } else {
                "Repo".into()
            },
        });
    }
    let branches = cat.entities_of(&s.id, "branch");
    if let Some(b) = branches
        .iter()
        .find(|b| b.origin == "recorded" && b.key != "main" && b.key != "master")
        .or(branches.first())
    {
        chips.push(Chip {
            label: b.key.clone(),
            href: format!("/e?kind=branch&key={}", enc(&b.key)),
            class: "chip branch",
            title: "Branch".into(),
        });
    }
    let mut prs = cat.entities_of(&s.id, "pr");
    prs.sort_by_key(|e| -e.strength);
    let repo = cat.repo_name(s).unwrap_or_default().to_lowercase();
    for p in prs.iter().filter(|p| p.strength >= 2).take(3) {
        let verb = match p.strength {
            4 => "Opened in this session",
            3 => "Linked to this session",
            _ => "Pushed for",
        };
        // Within the session's own repo, the number is enough.
        let label = match p.display.split_once('#') {
            Some((r, n)) if !repo.is_empty() && r.to_lowercase() == repo => format!("PR #{n}"),
            _ => p.display.clone(),
        };
        chips.push(Chip {
            label,
            href: format!("/e?kind=pr&key={}", enc(&p.key)),
            class: "chip pr",
            title: format!("{verb}: {}", p.display),
        });
    }
    for t in cat.entities_of(&s.id, "ticket").iter().take(3) {
        chips.push(Chip {
            label: t.display.clone(),
            href: format!("/e?kind=ticket&key={}", enc(&t.key.to_lowercase())),
            class: "chip ticket",
            title: "Ticket".into(),
        });
    }
    chips
}

/// Words to highlight: the query's own words, plus rewrite terms.
pub fn highlight_terms(text: &str, extra: &[String]) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "that", "this", "with", "where", "what", "when", "was", "were",
        "about", "from", "into", "our", "you",
    ];
    let mut out: Vec<String> = vec![];
    for w in text
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .chain(extra.iter().flat_map(|e| e.split_whitespace()))
    {
        let w = w.to_lowercase();
        if w.len() < 4 || STOP.contains(&w.as_str()) {
            continue;
        }
        let stem = crate::query::stem(&w);
        if !out.contains(&stem) {
            out.push(stem);
        }
    }
    out
}

/// Escapes `text` and marks words that start with a highlight term. With
/// `window`, returns only the densest stretch of that many characters.
pub fn highlight(text: &str, terms: &[String], window: Option<usize>) -> (String, usize) {
    let mut spans: Vec<(usize, usize)> = vec![];
    let lower = text.to_lowercase();
    if lower.len() == text.len() {
        let mut i = 0;
        for word in lower.split_inclusive(|c: char| !c.is_alphanumeric() && c != '_') {
            let w = word.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !w.is_empty()
                && terms
                    .iter()
                    .any(|t| w.starts_with(t.as_str()) && w.len() <= t.len() + 4)
            {
                spans.push((i, i + w.len()));
            }
            i += word.len();
        }
    }
    let hits = spans.len();
    let (mut from, mut to) = (0, text.len());
    if let Some(win) = window
        && text.len() > win
    {
        let mut best = (0, 0usize);
        for (j, (s, _)) in spans.iter().enumerate() {
            let n = spans[j..].iter().take_while(|(x, _)| *x < s + win).count();
            if n > best.1 {
                best = (*s, n);
            }
        }
        from = best.0.saturating_sub(win / 4);
        while !text.is_char_boundary(from) {
            from -= 1;
        }
        to = (from + win).min(text.len());
        while !text.is_char_boundary(to) {
            to += 1;
        }
    }
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    let mut pos = from;
    for (s, e) in spans.iter().filter(|(s, e)| *s >= from && *e <= to) {
        out.push_str(&esc(&text[pos..*s]));
        out.push_str("<mark>");
        out.push_str(&esc(&text[*s..*e]));
        out.push_str("</mark>");
        pos = *e;
    }
    out.push_str(&esc(&text[pos..to]));
    if to < text.len() {
        out.push('…');
    }
    (out, hits)
}

fn reason_label(r: &str) -> String {
    match r {
        "keyword" => "words".into(),
        "semantic" => "meaning".into(),
        "summary" => "summary".into(),
        "rewrite" => "related terms".into(),
        "repo" => "repo".into(),
        "time" => "date".into(),
        other => other.to_string(),
    }
}

/// Renders new bullet summaries while keeping existing prose summaries readable.
fn summary_html(text: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !lines.is_empty() && lines.iter().all(|l| l.starts_with("- ")) {
        let items: String = lines
            .iter()
            .map(|l| format!("<li>{}</li>", highlight(&l[2..], terms, None).0))
            .collect();
        format!("<ul>{items}</ul>")
    } else {
        highlight(text, terms, None).0
    }
}

fn result_view(
    cat: &Catalog,
    h: &Hit,
    texts: &std::collections::HashMap<i64, (i64, String, String)>,
    terms: &[String],
    q: &str,
) -> Option<ResultView> {
    let s = cat.sessions.get(&h.session)?;
    let mut snippets = vec![];
    for t in &h.turns {
        let Some((_, user, assistant)) = texts.get(t) else {
            continue;
        };
        let (u, uh) = highlight(user, terms, Some(220));
        let (a, ah) = highlight(assistant, terms, Some(220));
        let html = if uh >= ah && !user.is_empty() {
            format!("<span class=\"who\">You</span> {u}")
        } else {
            format!("<span class=\"who\">{}</span> {a}", agent_name(&s.source))
        };
        if !snippets.contains(&html) {
            snippets.push(html);
        }
        if snippets.len() == 2 {
            break;
        }
    }
    let blurb = s
        .summary
        .as_deref()
        .or(s.recap.as_deref())
        .map(|b| summary_html(&crate::model::clip_chars(b, 320), terms));
    let qs = if q.is_empty() {
        String::new()
    } else {
        format!("?q={}", enc(q))
    };
    let anchor = h
        .turns
        .first()
        .and_then(|t| texts.get(t))
        .map(|(idx, _, _)| format!("#t{idx}"))
        .unwrap_or_default();
    Some(ResultView {
        href: format!("/s/{}{qs}{anchor}", s.id),
        title: if s.title.is_empty() {
            "Untitled session".into()
        } else {
            s.title.clone()
        },
        agent: s.source.clone(),
        is_sub: s.parent_id.is_some(),
        chips: chips_for(cat, s),
        ago: fmt_ago(s.ended),
        when: fmt_date(s.ended),
        blurb,
        snippets,
        reasons: h.reasons.iter().map(|r| reason_label(r)).collect(),
        resume: crate::resume_command(s),
        cwd_gone: s
            .cwd
            .as_ref()
            .is_some_and(|c| !std::path::Path::new(c).exists()),
        session_id: s.id.clone(),
    })
}

pub fn agent_name(source: &str) -> &'static str {
    if source == "claude" {
        "Claude"
    } else {
        "Codex"
    }
}

fn facets(cat: &Catalog, p: &Params, hits: &[Hit]) -> Vec<FacetGroup> {
    let mut agent: BTreeMap<String, usize> = BTreeMap::new();
    let mut repo: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let mut kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut outcome: BTreeMap<String, usize> = BTreeMap::new();
    let now = chrono::Utc::now().timestamp();
    let (mut week, mut month) = (0, 0);
    for h in hits {
        let Some(s) = cat.sessions.get(&h.session) else {
            continue;
        };
        *agent.entry(s.source.clone()).or_default() += 1;
        if let Some(k) = &s.repo_key {
            repo.entry(k.clone())
                .or_insert((cat.repo_name(s).unwrap_or_default(), 0))
                .1 += 1;
        }
        if let Some(k) = &s.kind {
            *kind.entry(k.clone()).or_default() += 1;
        }
        if let Some(o) = &s.outcome {
            *outcome.entry(o.clone()).or_default() += 1;
        }
        if now - s.ended < 7 * 86_400 {
            week += 1;
        }
        if now - s.ended < 30 * 86_400 {
            month += 1;
        }
    }
    let active = |k: &str, v: &str| p.filters().iter().any(|(fk, fv)| *fk == k && *fv == v);
    let item = |k: &str, v: &str, label: String, count: usize| FacetItem {
        label,
        count,
        href: p.url("/", Some((k, v))),
        active: active(k, v),
    };
    let mut groups = vec![];
    let mut push = |name: &'static str, mut items: Vec<FacetItem>| {
        items.sort_by(|a, b| b.active.cmp(&a.active).then(b.count.cmp(&a.count)));
        items.truncate(8);
        if !items.is_empty() {
            groups.push(FacetGroup { name, items });
        }
    };
    push(
        "Agent",
        agent
            .into_iter()
            .map(|(k, n)| item("agent", &k, agent_name(&k).to_string(), n))
            .collect(),
    );
    push(
        "Repo",
        repo.into_iter()
            .map(|(k, (name, n))| item("repo", &k, name, n))
            .collect(),
    );
    push(
        "When",
        [("7d", "Last 7 days", week), ("30d", "Last 30 days", month)]
            .into_iter()
            .filter(|x| x.2 > 0)
            .map(|(v, l, n)| item("since", v, l.into(), n))
            .collect(),
    );
    push(
        "Kind",
        kind.into_iter()
            .map(|(k, n)| item("kind", &k, k.clone(), n))
            .collect(),
    );
    push(
        "Outcome",
        outcome
            .into_iter()
            .map(|(k, n)| item("outcome", &k, k.clone(), n))
            .collect(),
    );
    groups
}

fn render_results(
    e: &Engine,
    p: &Params,
    res: &Results,
    stream: bool,
    refining: bool,
) -> Result<String> {
    let cat = e.catalog();
    let c = e.conn()?;
    let shown: Vec<&Hit> = res.hits.iter().take(25).collect();
    let ids: Vec<i64> = shown
        .iter()
        .flat_map(|h| h.turns.iter().take(3).copied())
        .collect();
    let texts = crate::store::turn_texts(&c, &ids)?;
    let pq = e.parse(&p.full_query());
    let extra = res
        .rewrite
        .as_ref()
        .map(|r| r.terms.clone())
        .unwrap_or_default();
    let terms = highlight_terms(&pq.text, &extra);
    let results: Vec<ResultView> = shown
        .iter()
        .filter_map(|h| result_view(&cat, h, &texts, &terms, &p.q))
        .collect();
    let status = match res.stage {
        _ if p.is_empty() => format!("{} most recent sessions", results.len()),
        "keyword" => format!(
            "{} sessions with matching words. Ranking by meaning…",
            res.total
        ),
        "refined" => {
            let terms = res
                .rewrite
                .as_ref()
                .map(|r| r.terms.join(", "))
                .unwrap_or_default();
            format!("{} sessions, ranked. Also searched for: {terms}", res.total)
        }
        _ => format!("{} sessions, ranked in {} ms", res.total, res.took_ms),
    };
    let tpl = ResultsTpl {
        facets: if p.is_empty() { vec![] } else { facets(&cat, p, &res.hits) },
        empty_hint: results.is_empty().then(|| {
            if p.filters().is_empty() {
                "No session matches. Try fewer or different words, or describe what the session was about.".into()
            } else {
                "No session matches with these filters. Remove a filter to search wider.".into()
            }
        }),
        results,
        status,
        refining,
        notes: res.notes.clone(),
        stream_url: stream.then(|| p.url("/stream", None)),
        q: p.q.clone(),
    };
    Ok(tpl.render()?)
}

fn recent(e: &Engine) -> Results {
    let cat = e.catalog();
    let mut list: Vec<&SessionMeta> = cat
        .sessions
        .values()
        .filter(|s| s.parent_id.is_none())
        .collect();
    list.sort_by_key(|s| -s.ended);
    Results {
        hits: list
            .iter()
            .take(40)
            .map(|s| Hit {
                session: s.id.clone(),
                score: 0.0,
                reasons: vec![],
                turns: vec![],
                rerank: None,
            })
            .collect(),
        total: list.len(),
        stage: "recent",
        ..Default::default()
    }
}

/// The instant first stage, plus the SSE hookup for the later stages.
fn first_stage(e: &Engine, p: &Params) -> Result<String> {
    if p.is_empty() {
        return render_results(e, p, &recent(e), false, false);
    }
    let pq = e.parse(&p.full_query());
    let res = e.quick(&pq)?;
    let opts = e.default_options();
    let more = opts.vectors || opts.rerank || e.llm.rewrite_enabled();
    render_results(
        e,
        p,
        &res,
        more && !res.hits.is_empty(),
        more && !res.hits.is_empty(),
    )
}

async fn index(
    State(e): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let e2 = e.clone();
    let p2 = p.clone();
    let results_html = tokio::task::spawn_blocking(move || first_stage(&e2, &p2)).await??;
    let hidden = p
        .filters()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let active = p
        .filters()
        .into_iter()
        .map(|(k, v)| {
            let label = if k == "repo" {
                crate::extract::repo_display(v)
            } else {
                v.to_string()
            };
            Chip {
                label: format!("{k}: {label}"),
                href: p.url("/", Some((k, v))),
                class: "chip active",
                title: "Remove filter".into(),
            }
        })
        .collect();
    let sessions = e.catalog().sessions.len();
    Ok(Html(
        IndexTpl {
            q: p.q.clone(),
            hidden,
            active,
            results_html,
            sessions,
        }
        .render()?,
    ))
}

async fn search_fragment(
    State(e): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Response, AppError> {
    let url = p.url("/", None);
    let html = tokio::task::spawn_blocking(move || first_stage(&e, &p)).await??;
    let mut resp = Html(html).into_response();
    if let Ok(v) = HeaderValue::from_str(&url) {
        resp.headers_mut().insert("HX-Replace-Url", v);
    }
    Ok(resp)
}

async fn stream(
    State(e): State<AppState>,
    Query(p): Query<Params>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(4);
    tokio::spawn(async move {
        let send = |html: Result<String>| {
            let tx = tx.clone();
            async move {
                match html {
                    Ok(h) => {
                        let _ = tx.send(Event::default().event("results").data(h)).await;
                    }
                    Err(err) => tracing::warn!("stage failed: {err:#}"),
                }
            }
        };
        let (e2, p2) = (e.clone(), p.clone());
        let staged =
            tokio::task::spawn_blocking(move || -> Result<(crate::query::Parsed, Results)> {
                let pq = e2.parse(&p2.full_query());
                let res = e2.run(&pq, e2.default_options(), None, "hybrid")?;
                Ok((pq, res))
            })
            .await;
        let Ok(Ok((pq, res))) = staged else {
            let _ = tx.send(Event::default().event("done").data("end")).await;
            return;
        };
        let wants = e.wants_rewrite(&pq, &res);
        send(render_results(&e, &p, &res, false, wants)).await;
        if wants {
            match tokio::time::timeout(Duration::from_secs(5), e.rewrite(&pq)).await {
                Ok(Ok(rw)) if !rw.terms.is_empty() => {
                    let (e2, p2) = (e.clone(), p.clone());
                    let html = tokio::task::spawn_blocking(move || -> Result<String> {
                        let res = e2.run(&pq, e2.default_options(), Some(&rw), "refined")?;
                        render_results(&e2, &p2, &res, false, false)
                    })
                    .await
                    .unwrap_or_else(|j| Err(j.into()));
                    send(html).await;
                }
                other => {
                    if let Ok(Err(err)) = other {
                        tracing::warn!("rewrite skipped: {err:#}");
                    }
                    send(render_results(&e, &p, &res, false, false)).await;
                }
            }
        }
        // Browsers do not dispatch an SSE event with empty data, and without
        // this event htmx would not close the stream and EventSource would
        // reconnect, rerunning the search every few seconds.
        let _ = tx.send(Event::default().event("done").data("end")).await;
    });
    Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default())
}

// ---- session page ----

/// Parse Markdown before highlighting so marks never corrupt link destinations
/// or Markdown delimiters. Raw HTML is displayed literally; the final allowlist
/// also removes unsafe Markdown URLs and remote images from archived messages.
fn markdown(text: &str, terms: &[String]) -> (String, usize) {
    use pulldown_cmark::{Event, Options, Parser, html};
    static SANITIZER: std::sync::LazyLock<ammonia::Builder<'static>> =
        std::sync::LazyLock::new(|| {
            let mut builder = ammonia::Builder::default();
            builder.add_tags(&["mark"]).rm_tags(&["img"]);
            builder
        });
    if text.is_empty() {
        return (String::new(), 0);
    }
    let mut hits = 0;
    let events = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(
        |event| {
            let (text, code) = match event {
                Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => (t, false),
                Event::Code(t) => (t, true),
                other => return other,
            };
            let (mut rendered, n) = if terms.is_empty() {
                (esc(&text), 0)
            } else {
                highlight(&text, terms, None)
            };
            hits += n;
            if code {
                rendered = format!("<code>{rendered}</code>");
            }
            Event::Html(rendered.into())
        },
    );
    let mut html = String::with_capacity(text.len());
    html::push_html(&mut html, events);
    (SANITIZER.clean(&html).to_string(), hits)
}

pub struct ToolView {
    pub name: String,
    pub input: String,
    pub output: String,
    pub error: bool,
}

pub struct TurnView {
    pub idx: i64,
    pub when: String,
    pub user: String,
    pub assistant: String,
    pub tools: Vec<ToolView>,
    pub matched: bool,
    pub branch: Option<String>,
}

pub struct EntityGroup {
    pub name: &'static str,
    pub items: Vec<(String, String, String)>,
}

#[derive(Template)]
#[template(path = "session.html")]
struct SessionTpl {
    title: String,
    agent: String,
    summary: Option<String>,
    chips: Vec<Chip>,
    cwd: String,
    cwd_gone: bool,
    started: String,
    ended: String,
    model: String,
    resume: String,
    turns: Vec<TurnView>,
    matches: Vec<(i64, String)>,
    entities: Vec<EntityGroup>,
    parent: Option<(String, String)>,
    children: Vec<(String, String)>,
    q: String,
    back: String,
}

#[derive(Deserialize)]
struct SessionParams {
    #[serde(default)]
    q: String,
}

async fn session_page(
    State(e): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(sp): Query<SessionParams>,
) -> Result<Response, AppError> {
    let html = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let cat = e.catalog();
        let Some(s) = cat.sessions.get(&id) else {
            return Ok(None);
        };
        let c = e.conn()?;
        let pq = e.parse(&sp.q);
        let terms = if sp.q.is_empty() {
            vec![]
        } else {
            highlight_terms(&pq.text, &[])
        };
        let mut matches = vec![];
        let mut turns = vec![];
        for t in crate::store::turns(&c, &id)? {
            let (user, uh) = markdown(&t.user_text, &terms);
            let (assistant, ah) = markdown(&t.assistant_text, &terms);
            let tools: Vec<ToolView> =
                serde_json::from_str::<Vec<serde_json::Value>>(&t.tools_json)
                    .unwrap_or_default()
                    .iter()
                    .map(|v| ToolView {
                        name: v["name"].as_str().unwrap_or("").into(),
                        input: v["input"].as_str().unwrap_or("").into(),
                        output: v["output"].as_str().unwrap_or("").into(),
                        error: v["error"].as_bool().unwrap_or(false),
                    })
                    .collect();
            let matched = uh + ah > 0;
            if matched {
                let label = if !t.user_text.is_empty() {
                    &t.user_text
                } else {
                    &t.assistant_text
                };
                matches.push((
                    t.idx,
                    crate::model::clip_chars(label.lines().next().unwrap_or(""), 70),
                ));
            }
            turns.push(TurnView {
                idx: t.idx,
                when: t.ts.map(fmt_date).unwrap_or_default(),
                user,
                assistant,
                tools,
                matched,
                branch: t.branch,
            });
        }
        let mut groups: BTreeMap<&str, Vec<(String, String, String)>> = BTreeMap::new();
        for en in cat.entities.get(&id).into_iter().flatten() {
            let name = match en.kind.as_str() {
                "pr" => "Pull requests",
                "branch" => "Branches",
                "ticket" => "Tickets",
                "commit" => "Commits",
                "worktree" => "Directories",
                "repo" => "Repos mentioned",
                "file" => "Files",
                _ => continue,
            };
            let how = match (en.kind.as_str(), en.strength) {
                ("pr", 4) => "opened here".to_string(),
                ("pr", 3) => "linked by the agent".to_string(),
                ("pr", 2) => "pushed for".to_string(),
                ("pr", _) => "mentioned".to_string(),
                _ if en.origin == "parsed" => "from command output".to_string(),
                _ if en.origin == "inferred" => "inferred".to_string(),
                _ => String::new(),
            };
            let href = match en.kind.as_str() {
                "worktree" | "file" | "commit" => String::new(),
                k => format!("/e?kind={k}&key={}", enc(&en.key.to_lowercase())),
            };
            groups
                .entry(name)
                .or_default()
                .push((en.display.clone(), how, href));
        }
        let order = [
            "Pull requests",
            "Tickets",
            "Branches",
            "Commits",
            "Directories",
            "Files",
            "Repos mentioned",
        ];
        let entities = order
            .iter()
            .filter_map(|n| {
                groups.remove(n).map(|mut items| {
                    items.truncate(if *n == "Files" { 30 } else { 15 });
                    EntityGroup { name: n, items }
                })
            })
            .collect();
        let link = |sid: &str| {
            cat.sessions.get(sid).map(|x| {
                (
                    format!("/s/{}", x.id),
                    if x.title.is_empty() {
                        x.id.clone()
                    } else {
                        x.title.clone()
                    },
                )
            })
        };
        let tpl = SessionTpl {
            title: if s.title.is_empty() {
                "Untitled session".into()
            } else {
                s.title.clone()
            },
            agent: s.source.clone(),
            summary: s
                .summary
                .as_deref()
                .or(s.recap.as_deref())
                .map(|s| summary_html(s, &[])),
            chips: chips_for(&cat, s),
            cwd: s.cwd.clone().unwrap_or_default(),
            cwd_gone: s
                .cwd
                .as_ref()
                .is_some_and(|c| !std::path::Path::new(c).exists()),
            started: fmt_date(s.started),
            ended: fmt_date(s.ended),
            model: s.model.clone().unwrap_or_default(),
            resume: crate::resume_command(s),
            turns,
            matches,
            entities,
            parent: s.parent_id.as_deref().and_then(link),
            children: cat
                .children
                .get(&id)
                .map(|v| v.iter().filter_map(|c| link(c)).collect())
                .unwrap_or_default(),
            back: if sp.q.is_empty() {
                "/".into()
            } else {
                format!("/?q={}", enc(&sp.q))
            },
            q: sp.q,
        };
        Ok(Some(tpl.render()?))
    })
    .await??;
    Ok(match html {
        Some(h) => Html(h).into_response(),
        None => (StatusCode::NOT_FOUND, "No session with that ID.").into_response(),
    })
}

// ---- entity pages ----

#[derive(Template)]
#[template(path = "entity.html")]
struct EntityTpl {
    heading: String,
    kind: String,
    results_html: String,
    search_href: Option<String>,
}

#[derive(Deserialize)]
struct EntityParams {
    kind: String,
    key: String,
}

async fn entity_page(
    State(e): State<AppState>,
    Query(ep): Query<EntityParams>,
) -> Result<Html<String>, AppError> {
    let html = tokio::task::spawn_blocking(move || -> Result<String> {
        let cat = e.catalog();
        let mut ids: Vec<String> = if ep.kind == "repo" {
            cat.sessions
                .values()
                .filter(|s| s.repo_key.as_deref() == Some(ep.key.as_str()))
                .map(|s| s.id.clone())
                .collect()
        } else {
            cat.by_entity
                .get(&(ep.kind.clone(), ep.key.to_lowercase()))
                .cloned()
                .unwrap_or_default()
        };
        ids.sort_by_key(|id| -cat.sessions.get(id).map_or(0, |s| s.ended));
        ids.dedup();
        let display = match ep.kind.as_str() {
            "repo" => crate::extract::repo_display(&ep.key),
            _ => cat
                .entities
                .values()
                .flatten()
                .find(|x| x.kind == ep.kind && x.key.eq_ignore_ascii_case(&ep.key))
                .map(|x| x.display.clone())
                .unwrap_or(ep.key.clone()),
        };
        let res = Results {
            hits: ids
                .iter()
                .map(|id| Hit {
                    session: id.clone(),
                    score: 0.0,
                    reasons: vec![],
                    turns: vec![],
                    rerank: None,
                })
                .collect(),
            total: ids.len(),
            stage: "entity",
            ..Default::default()
        };
        let p = Params::default();
        let mut results_html = render_results(&e, &p, &res, false, false)?;
        if ids.is_empty() {
            results_html = "<p class=\"empty\">No sessions reference this.</p>".into();
        }
        let kind = match ep.kind.as_str() {
            "repo" => "Repo",
            "pr" => "Pull request",
            "ticket" => "Ticket",
            "branch" => "Branch",
            _ => "Entity",
        };
        Ok(EntityTpl {
            heading: display,
            kind: format!("{kind}, {} sessions", ids.len()),
            results_html,
            search_href: (ep.kind == "repo" || ep.kind == "branch")
                .then(|| format!("/?{}={}", ep.kind, enc(&ep.key))),
        }
        .render()?)
    })
    .await??;
    Ok(Html(html))
}

#[derive(Deserialize)]
struct FeedbackForm {
    q: String,
    session: String,
    label: String,
}

/// "This was the one" / "Not this" on a result adds it to the eval set.
async fn feedback(
    State(e): State<AppState>,
    Form(f): Form<FeedbackForm>,
) -> Result<Html<String>, AppError> {
    let label = if f.label == "good" { "good" } else { "bad" };
    let c = e.conn()?;
    c.execute(
        "INSERT INTO feedback (ts, query, session_id, label) VALUES (?, ?, ?, ?)",
        rusqlite::params![chrono::Utc::now().timestamp(), f.q, f.session, label],
    )?;
    Ok(Html(format!(
        "<span class=\"saved\">{}</span>",
        if label == "good" {
            "Saved as the right session"
        } else {
            "Saved as a wrong result"
        }
    )))
}

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("{:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_markdown_formats_text_and_preserves_search() {
        let text = "# Ordering\n\nA **stable** [ordering guide](https://example.com/ordering?q=1&n=2).\n\n- first\n- second\n\n1. third\n2. fourth\n\n> quoted\n\n`ordering <tag>`\n\n```rust\nlet ordering = \"<script>\";\n```\n\n| Key | Value |\n| --- | --- |\n| ordering | stable |";
        let (html, hits) = markdown(text, &highlight_terms("ordering", &[]));
        assert_eq!(hits, 5, "{html}");
        for expected in [
            "<h1><mark>Ordering</mark></h1>",
            "<strong>stable</strong>",
            "<ul>",
            "<ol>",
            "<blockquote>",
            "<table>",
            "<th>Key</th>",
            "<td><mark>ordering</mark></td>",
            "<code><mark>ordering</mark> &lt;tag&gt;</code>",
            "<pre><code>let <mark>ordering</mark> = \"&lt;script&gt;\";",
            "href=\"https://example.com/ordering?q=1&amp;n=2\"",
        ] {
            assert!(html.contains(expected), "missing {expected}: {html}");
        }
        assert_eq!(markdown("", &[]), (String::new(), 0));
    }

    #[test]
    fn transcript_markdown_cannot_inject_html_or_unsafe_urls() {
        let text = r#"<script>alert(1)</script>

<img src=x onerror=alert(2)><svg onload=alert(3)></svg>

[bad](javascript:alert%281%29) [encoded](jav&#x61;script:alert%282%29)
[data](data:text/html,boom) [vb](vbscript:msgbox)
![tracking](https://example.com/tracker.png)
[safe](https://example.com) [mail](mailto:hello@example.com)

<iframe srcdoc="boom"></iframe>
<a href="javascript:alert(4)">raw</a>"#;
        let (html, _) = markdown(text, &[]);
        for forbidden in [
            "<script",
            "<img",
            "<svg",
            "<iframe",
            "<a href=\"javascript:",
            "<a href=\"data:",
            "<a href=\"vbscript:",
        ] {
            assert!(!html.contains(forbidden), "found {forbidden}: {html}");
        }
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(html.contains("href=\"https://example.com\""), "{html}");
        assert!(html.contains("href=\"mailto:hello@example.com\""), "{html}");
    }

    #[test]
    fn highlight_marks_word_starts_and_escapes() {
        let terms = highlight_terms("fixed ordering <b>", &[]);
        let (h, n) = highlight("Fix the <ordering> bug; reorder later", &terms, None);
        assert_eq!(n, 2, "{h}");
        assert!(h.contains("<mark>Fix</mark>"), "{h}");
        assert!(h.contains("&lt;<mark>ordering</mark>&gt;"), "{h}");
        assert!(!h.contains("<mark>reorder"), "{h}");
    }

    #[test]
    fn session_summary_renders_bullets_and_escapes_content() {
        for (summary, bullet_count) in [
            (
                "- Fixed <script>alert(1)</script>.\n\n- Tests passed & staging is pending.",
                2,
            ),
            (
                "Fixed <script>alert(1)</script>. Tests passed & staging is pending.",
                0,
            ),
        ] {
            let html = SessionTpl {
                title: "Summary example".into(),
                agent: "claude".into(),
                summary: Some(summary_html(summary, &[])),
                chips: vec![],
                cwd: String::new(),
                cwd_gone: false,
                started: String::new(),
                ended: String::new(),
                model: String::new(),
                resume: String::new(),
                turns: vec![],
                matches: vec![],
                entities: vec![],
                parent: None,
                children: vec![],
                q: String::new(),
                back: "/".into(),
            }
            .render()
            .unwrap();
            assert_eq!(html.matches("<li>").count(), bullet_count);
            assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
            assert!(html.contains("Tests passed &amp; staging is pending."));
            assert!(!html.contains("<script>alert(1)</script>"));
        }
    }

    #[test]
    fn facet_links_toggle_one_filter() {
        let p = Params {
            q: "flaky test".into(),
            agent: Some("codex".into()),
            ..Default::default()
        };
        assert_eq!(p.url("/", Some(("agent", "codex"))), "/?q=flaky+test");
        assert_eq!(
            p.url("/", Some(("agent", "claude"))),
            "/?q=flaky+test&agent=claude"
        );
        assert_eq!(
            p.url("/", Some(("repo", "github.com/a/b"))),
            "/?q=flaky+test&agent=codex&repo=github.com%2Fa%2Fb"
        );
    }
}
