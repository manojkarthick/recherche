//! The web UI: server-rendered askama templates, made interactive with htmx.
//! Results stream in stages over SSE: keyword results first, then hybrid
//! and reranked, then (when it helps) results refined by an LLM rewrite.

use crate::queue::{Task, TaskState};
use crate::search::{Engine, Hit, Results};
use crate::store::{Catalog, SessionMeta};
use crate::summarize::Pending;
use crate::util::{fmt_ago, fmt_date, fmt_day, fmt_duration, fmt_time};
use anyhow::Result;
use askama::Template;
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use regex::Regex;
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

type AppState = Arc<Engine>;

pub async fn serve(engine: Arc<Engine>, listen: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("listening on http://{listen}");
    axum::serve(listener, router(engine)).await?;
    Ok(())
}

pub(crate) fn router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/search", get(search_fragment))
        .route("/stream", get(stream))
        .route("/s/{*id}", get(session_page))
        .route(
            "/session/summarize",
            get(summarize_status).post(summarize_start),
        )
        .route("/session/reindex", get(reindex_status).post(reindex_start))
        .route("/tasks/all", get(all_tasks_status))
        .route("/tasks/reindex", post(reindex_all_start))
        .route("/tasks/summarize", post(summarize_all_start))
        .route("/tasks/summarize/cancel", post(summarize_all_cancel))
        .route("/e", get(entity_page))
        .route("/feedback", post(feedback))
        .route("/assets/{*path}", get(asset))
        .nest("/api", crate::api::router())
        .layer(middleware::from_fn_with_state(engine.clone(), auth))
        .with_state(engine)
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
                Some("woff2") => "font/woff2",
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
    pub(crate) fn full_query(&self) -> String {
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

/// Escapes a path-like string and lets it wrap after each `/`. Copied text
/// stays the same: `<wbr>` adds no characters.
pub fn wbr(s: &str) -> String {
    esc(s).replace('/', "/<wbr>")
}

// ---- view models ----

pub struct Chip {
    pub label: String,
    pub href: String,
    pub class: &'static str,
    pub title: String,
    /// The matching GitHub page, shown on the session page as an external link.
    pub github: Option<String>,
    /// The search matched on this ref.
    pub hit: bool,
    /// Refs of the session's repo share line 0; each other repo whose PRs
    /// the session touched gets a line of its own.
    pub line: u8,
}

/// Chips split into their lines. Always at least one line, which also holds
/// the notes.
pub fn chip_lines(chips: &[Chip]) -> Vec<&[Chip]> {
    if chips.is_empty() {
        return vec![&[]];
    }
    chips.chunk_by(|a, b| a.line == b.line).collect()
}

/// The GitHub page for a PR, repo or branch entity key. A branch needs the
/// session's repo key. Other hosts and `path:` repos have no page.
pub fn github_url(kind: &str, key: &str, repo_key: Option<&str>) -> Option<String> {
    // `owner/repo`, the form GitHub keys use without their host.
    let repo = |r: &str| {
        (r.split('/').count() == 2 && !r.split('/').any(str::is_empty)).then(|| r.to_string())
    };
    match kind {
        "pr" => {
            let (r, n) = key.split_once('#')?;
            let n = n.parse::<u64>().ok()?;
            Some(format!("https://github.com/{}/pull/{n}", repo(r)?))
        }
        "repo" => Some(format!(
            "https://github.com/{}",
            repo(key.strip_prefix("github.com/")?)?
        )),
        "branch" => {
            let r = repo(repo_key?.strip_prefix("github.com/")?)?;
            let path: Vec<String> = key.split('/').map(enc).collect();
            Some(format!("https://github.com/{r}/tree/{}", path.join("/")))
        }
        _ => None,
    }
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
    /// The date is one of the reasons it matched.
    pub time_hit: bool,
    /// Reasons with nothing on the result to highlight, shown in the margin.
    pub notes: Vec<String>,
    pub resume: String,
    pub cwd_gone: bool,
    pub session_id: String,
    /// Tooltip for the "In progress" badge, set while a session that will
    /// be summarized is still active.
    pub in_progress: Option<String>,
}

pub struct FacetItem {
    pub label: String,
    /// The full name, when the label is shortened.
    pub title: String,
    pub count: usize,
    pub href: String,
    pub hx_href: String,
    pub active: bool,
    /// Agent logo class, empty for other facets.
    pub logo: &'static str,
}

pub struct FacetGroup {
    pub name: &'static str,
    pub items: Vec<FacetItem>,
}

pub struct FilterChip {
    pub label: String,
    pub href: String,
    pub hx_href: String,
}

#[derive(Template)]
#[template(path = "results.html")]
struct ResultsTpl {
    results: Vec<ResultView>,
    facets: Vec<FacetGroup>,
    hidden: Vec<(String, String)>,
    active: Vec<FilterChip>,
    status: String,
    refining: bool,
    notes: Vec<String>,
    stream_url: Option<String>,
    q: String,
    empty_hint: Option<String>,
}

#[derive(Template)]
#[template(path = "all_tasks.html")]
struct AllTasksTpl {
    summaries_enabled: bool,
    all_poll: bool,
    all_since: u64,
    reindex_all_busy: bool,
    reindex_all_label: &'static str,
    summarize_all_busy: bool,
    summarize_all_label: String,
    all_done: Option<String>,
    all_err: Option<String>,
    oob_sessions: Option<usize>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTpl {
    q: String,
    results_html: String,
    sessions: usize,
    summaries_enabled: bool,
    all_poll: bool,
    all_since: u64,
    reindex_all_busy: bool,
    reindex_all_label: &'static str,
    summarize_all_busy: bool,
    summarize_all_label: String,
    all_done: Option<String>,
    all_err: Option<String>,
    oob_sessions: Option<usize>,
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
            github: github_url("repo", k, None),
            hit: false,
            line: 0,
        });
    }
    if let Some(branch) = cat.primary_branch(&s.id) {
        chips.push(Chip {
            label: branch.to_string(),
            href: format!("/e?kind=branch&key={}", enc(branch)),
            class: "chip branch",
            title: "Branch".into(),
            github: github_url("branch", branch, s.repo_key.as_deref()),
            hit: false,
            line: 0,
        });
    }
    let mut prs = cat.entities_of(&s.id, "pr");
    prs.sort_by_key(|e| -e.strength);
    let repo = cat.repo_name(s).unwrap_or_default().to_lowercase();
    // PRs in other repos, grouped by repo in order of their strongest PR.
    let mut others: Vec<(String, Vec<Chip>)> = vec![];
    for p in prs.iter().filter(|p| p.strength >= 2).take(3) {
        let verb = match p.strength {
            4 => "Opened in this session",
            3 => "Linked to this session",
            _ => "Pushed for",
        };
        let chip = |label| Chip {
            label,
            href: format!("/e?kind=pr&key={}", enc(&p.key)),
            class: "chip pr",
            title: format!("{verb}: {}", p.display),
            github: github_url("pr", &p.key, None),
            hit: false,
            line: 0,
        };
        match p.display.split_once('#') {
            Some((r, n)) if repo.is_empty() || r.to_lowercase() != repo => {
                let chip = chip(format!("PR #{n}"));
                match others.iter_mut().find(|(o, _)| o.eq_ignore_ascii_case(r)) {
                    Some((_, c)) => c.push(chip),
                    None => others.push((r.to_string(), vec![chip])),
                }
            }
            Some((_, n)) => chips.push(chip(format!("PR #{n}"))),
            None => chips.push(chip(p.display.clone())),
        }
    }
    for t in cat.entities_of(&s.id, "ticket").iter().take(3) {
        chips.push(Chip {
            label: t.display.clone(),
            href: format!("/e?kind=ticket&key={}", enc(&t.key.to_lowercase())),
            class: "chip ticket",
            title: "Ticket".into(),
            github: None,
            hit: false,
            line: 0,
        });
    }
    for (i, (r, prs)) in others.into_iter().enumerate() {
        let line = i as u8 + 1;
        let key = format!("github.com/{}", r.to_lowercase());
        chips.push(Chip {
            label: r,
            href: format!("/e?kind=repo&key={}", enc(&key)),
            class: "chip repo",
            title: "Repo".into(),
            github: github_url("repo", &key, None),
            hit: false,
            line,
        });
        chips.extend(prs.into_iter().map(|c| Chip { line, ..c }));
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

/// Shows each match reason where it applies: a highlight on the matching ref
/// or the date, or a margin note when the result has nothing to point at.
/// Matched words are already marked in the summary and snippets. Returns
/// whether the date matched, and the notes.
fn place_reasons(reasons: &[String], chips: &mut [Chip]) -> (bool, Vec<String>) {
    let words = reasons.iter().any(|r| r == "keyword" || r == "summary");
    let mut time_hit = false;
    let mut notes: Vec<String> = vec![];
    for r in reasons {
        let (kind, label) = match r.as_str() {
            "keyword" | "summary" => continue,
            "semantic" => {
                if !words {
                    notes.push("similar meaning".into());
                }
                continue;
            }
            "rewrite" => {
                notes.push("related terms".into());
                continue;
            }
            "time" => {
                time_hit = true;
                continue;
            }
            "repo" => ("repo", ""),
            // "branch X" from a branch boost, "exact KIND LABEL" from an exact entity.
            other => other
                .strip_prefix("exact ")
                .unwrap_or(other)
                .split_once(' ')
                .unwrap_or((other, "")),
        };
        // PR labels are "PR #12" or "owner/repo#12" depending on context.
        let num = |l: &str| l.rsplit_once('#').map(|(_, n)| n.to_string());
        let found = chips.iter_mut().find(|c| {
            c.class.split(' ').nth(1) == Some(kind)
                && match kind {
                    "repo" => true,
                    "pr" => num(&c.label).is_some() && num(&c.label) == num(label),
                    _ => c.label.eq_ignore_ascii_case(label),
                }
        });
        match found {
            Some(c) => c.hit = true,
            None if matches!(kind, "pr" | "ticket") || label.is_empty() => {
                notes.push(if label.is_empty() { kind } else { label }.to_string())
            }
            None => notes.push(format!("{kind} {label}")),
        }
    }
    notes.dedup();
    (time_hit, notes)
}

/// Drops Markdown syntax that reads as noise in a short excerpt: emphasis,
/// heading and quote markers, and link targets. Runs before highlighting.
fn plain_excerpt(text: &str) -> String {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"!?\[([^\]\n]*)\]\((?:<[^>\n]*>|[^)\s]*)\)").unwrap());
    static MARKERS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)\*\*|^[ \t]*(?:#{1,6}|>)[ \t]+").unwrap());
    code_marks(&MARKERS.replace_all(&LINK.replace_all(text, "$1"), ""))
}

const CODE_OPEN: char = '\u{E000}';
const CODE_CLOSE: char = '\u{E001}';

/// Pairs `code` backticks before any clipping, as private-use marks that
/// survive highlighting. Pairing after a clip would match the wrong backticks
/// when the clip lands inside a span.
fn code_marks(text: &str) -> String {
    static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
    CODE.replace_all(text, format!("{CODE_OPEN}$1{CODE_CLOSE}"))
        .replace('`', "")
}

/// Turns the marks from `code_marks` into <code> elements, closing a span a
/// clip cut open at either end.
fn code_spans(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 16);
    let mut open = false;
    for c in html.chars() {
        match c {
            CODE_OPEN if !open => {
                out.push_str("<code>");
                open = true;
            }
            CODE_CLOSE if open => {
                out.push_str("</code>");
                open = false;
            }
            // A close with no open: the clip removed the start of the span.
            CODE_CLOSE => {
                let at = if out.starts_with('…') {
                    '…'.len_utf8()
                } else {
                    0
                };
                out.insert_str(at, "<code>");
                out.push_str("</code>");
            }
            CODE_OPEN => {}
            _ => out.push(c),
        }
    }
    if open {
        out.push_str("</code>");
    }
    out
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
            .map(|l| {
                format!(
                    "<li>{}</li>",
                    code_spans(&highlight(&code_marks(&l[2..]), terms, None).0)
                )
            })
            .collect();
        format!("<ul>{items}</ul>")
    } else {
        code_spans(&highlight(&code_marks(text), terms, None).0)
    }
}

/// The summary under a result: whole bullets up to `budget` bytes, so the list
/// never ends on a fragment. A long first bullet or a prose summary is clipped.
fn blurb_html(text: &str, terms: &[String], budget: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() || !lines.iter().all(|l| l.starts_with("- ")) {
        return summary_html(&crate::model::clip_chars(text, budget), terms);
    }
    let mut kept = vec![];
    let mut used = 0;
    for l in &lines {
        if !kept.is_empty() && used + l.len() > budget {
            break;
        }
        used += l.len();
        kept.push(crate::model::clip_chars(l, budget));
    }
    summary_html(&kept.join("\n"), terms)
}

/// The user's prompt as shown, without `<system_instruction>` blocks when
/// `ui.hide_system_instructions` is set.
fn shown_prompt<'a>(e: &Engine, text: &'a str) -> std::borrow::Cow<'a, str> {
    if e.cfg.ui.hide_system_instructions {
        crate::util::strip_system_instructions(text)
    } else {
        text.into()
    }
}

fn result_view(
    e: &Engine,
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
        let user = shown_prompt(e, user);
        let (u, uh) = highlight(&plain_excerpt(&user), terms, Some(220));
        let (a, ah) = highlight(&plain_excerpt(assistant), terms, Some(220));
        let html = if uh >= ah && !user.is_empty() {
            format!("<span class=\"who\">You</span> {}", code_spans(&u))
        } else {
            format!(
                "<span class=\"who logo {}\" role=\"img\" aria-label=\"{}\" title=\"{}\"></span> {}",
                crate::parse::source(&s.source),
                agent_name(&s.source),
                agent_name(&s.source),
                code_spans(&a)
            )
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
        // Matching excerpts already explain the result, so the summary shrinks.
        .map(|b| blurb_html(b, terms, if snippets.is_empty() { 320 } else { 180 }));
    let qs = if q.is_empty() {
        String::new()
    } else {
        format!("?q={}", enc(q))
    };
    // Hits are ranked by score; open the session at the earliest one.
    let anchor = h
        .turns
        .iter()
        .filter_map(|t| texts.get(t))
        .map(|(idx, _, _)| *idx)
        .min()
        .map(|idx| format!("#t{idx}"))
        .unwrap_or_default();
    let mut chips = chips_for(cat, s);
    let (time_hit, notes) = place_reasons(&h.reasons, &mut chips);
    Some(ResultView {
        href: format!("/s/{}{qs}{anchor}", s.id),
        title: if s.title.is_empty() {
            "Untitled session".into()
        } else {
            s.title.clone()
        },
        agent: s.source.clone(),
        is_sub: s.parent_id.is_some(),
        chips,
        ago: fmt_ago(s.ended),
        when: fmt_date(s.ended),
        blurb,
        snippets,
        time_hit,
        notes,
        resume: crate::resume_command(s),
        cwd_gone: s
            .cwd
            .as_ref()
            .is_some_and(|c| !std::path::Path::new(c).exists()),
        session_id: s.id.clone(),
        in_progress: (pending(e, cat, s) == Some(Pending::InProgress)).then(|| {
            format!(
                "No summary yet. Sessions are summarized after {} minutes without any activity.",
                e.cfg.llm.idle_minutes
            )
        }),
    })
}

/// Why a session has no summary yet, when it is expected to get one.
fn pending(e: &Engine, cat: &Catalog, s: &SessionMeta) -> Option<Pending> {
    if !e.llm.summaries_enabled() {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    crate::summarize::pending(&e.cfg, cat.auto_summary_since, s, now)
}

/// The session page's stand-in for a missing summary.
fn pending_note(p: Pending, idle_minutes: i64) -> String {
    match p {
        Pending::InProgress => format!(
            "This session is in progress. Sessions are summarized after {idle_minutes} minutes without any activity."
        ),
        Pending::Queued => "Summary queued. The background pass runs every few minutes.".into(),
        Pending::Backfill => "Not summarized automatically because this session started before recherche serve first ran. Run recherche summarize to add it.".into(),
    }
}

/// The product name, used where agents are filtered rather than quoted.
pub fn agent_product(source: &str) -> &'static str {
    match source {
        "claude" => "Claude Code",
        "antigravity" => "Antigravity",
        "cursor" => "Cursor",
        _ => "Codex",
    }
}

pub fn agent_name(source: &str) -> &'static str {
    match source {
        "claude" => "Claude",
        "antigravity" => "Antigravity",
        "cursor" => "Cursor",
        _ => "Codex",
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
        title: String::new(),
        count,
        href: p.url("/", Some((k, v))),
        hx_href: p.url("/search", Some((k, v))),
        active: active(k, v),
        logo: "",
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
            .map(|(k, n)| FacetItem {
                logo: crate::parse::source(&k),
                ..item("agent", &k, agent_product(&k).to_string(), n)
            })
            .collect(),
    );
    push("Repo", {
        // The column is narrow, so show the repo name without its owner,
        // unless two owners have a repo with the same name.
        let short = |n: &str| n.rsplit_once('/').map_or(n, |(_, r)| r).to_string();
        let names: Vec<String> = repo.values().map(|(n, _)| short(n)).collect();
        repo.into_iter()
            .map(|(k, (name, n))| {
                let s = short(&name);
                if s == name || names.iter().filter(|x| **x == s).count() > 1 {
                    return item("repo", &k, name, n);
                }
                FacetItem {
                    title: name,
                    ..item("repo", &k, s, n)
                }
            })
            .collect()
    });
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
    // The entity page leaves its own ref off each result; the heading names it.
    hide_ref: Option<&str>,
) -> Result<String> {
    let cat = e.catalog();
    let c = e.conn()?;
    let shown: Vec<&Hit> = res.hits.iter().take(25).collect();
    let ids: Vec<i64> = shown
        .iter()
        // All hit turns, not just the excerpted ones, so the link can open at the earliest.
        .flat_map(|h| h.turns.iter().copied())
        .collect();
    let texts = crate::store::turn_texts(&c, &ids)?;
    let pq = e.parse(&p.full_query());
    let extra = res
        .rewrite
        .as_ref()
        .map(|r| r.terms.clone())
        .unwrap_or_default();
    let terms = highlight_terms(&pq.text, &extra);
    let mut results: Vec<ResultView> = shown
        .iter()
        .filter_map(|h| result_view(e, &cat, h, &texts, &terms, &p.q))
        .collect();
    if let Some(href) = hide_ref {
        for r in &mut results {
            r.chips.retain(|c| c.href != href);
        }
    }
    let status = match res.stage {
        "entity" => "Newest first".into(),
        _ if p.is_empty() => format!("{} most recent sessions", results.len()),
        "keyword" if pq.text.trim().is_empty() && pq.phrases.is_empty() => {
            format!("{} sessions", res.total)
        }
        "keyword" if stream => format!(
            "{} sessions with matching words. Ranking by meaning…",
            res.total
        ),
        "keyword" => format!("{} sessions with matching words", res.total),
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
    let hidden = p
        .filters()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let active = p
        .filters()
        .into_iter()
        .map(|(k, v)| {
            let label = match k {
                "repo" => crate::extract::repo_display(v),
                "agent" => agent_product(v).to_string(),
                _ => v.to_string(),
            };
            FilterChip {
                label: format!("{k}: {label}"),
                href: p.url("/", Some((k, v))),
                hx_href: p.url("/search", Some((k, v))),
            }
        })
        .collect();
    let tpl = ResultsTpl {
        facets: if p.is_empty() {
            vec![]
        } else {
            facets(&cat, p, &res.hits)
        },
        hidden,
        active,
        empty_hint: (results.is_empty() && !stream).then(|| {
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
        return render_results(e, p, &recent(e), false, false, None);
    }
    let pq = e.parse(&p.full_query());
    let res = e.quick(&pq)?;
    let opts = e.default_options();
    let more = (!pq.text.trim().is_empty() || !pq.phrases.is_empty())
        && (opts.vectors || opts.rerank || e.llm.rewrite_enabled());
    render_results(e, p, &res, more, more, None)
}

async fn index(
    State(e): State<AppState>,
    Query(p): Query<Params>,
) -> Result<Html<String>, AppError> {
    let e2 = e.clone();
    let p2 = p.clone();
    let results_html = tokio::task::spawn_blocking(move || first_stage(&e2, &p2)).await??;
    let sessions = e.catalog().sessions.len();
    let tasks = all_tasks_view(
        &e,
        e.tasks.page_state(&Task::ReindexAll, TASK_ERR_TTL),
        e.tasks.page_state(&Task::SummarizeAll, TASK_ERR_TTL),
        e.tasks.completed_seq(),
        None,
    );
    Ok(Html(
        IndexTpl {
            q: p.q.clone(),
            results_html,
            sessions,
            summaries_enabled: tasks.summaries_enabled,
            all_poll: tasks.all_poll,
            all_since: tasks.all_since,
            reindex_all_busy: tasks.reindex_all_busy,
            reindex_all_label: tasks.reindex_all_label,
            summarize_all_busy: tasks.summarize_all_busy,
            summarize_all_label: tasks.summarize_all_label,
            all_done: tasks.all_done,
            all_err: tasks.all_err,
            oob_sessions: None,
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
        send(render_results(&e, &p, &res, false, wants, None)).await;
        if wants {
            match tokio::time::timeout(Duration::from_secs(5), e.rewrite(&pq)).await {
                Ok(Ok(rw)) if !rw.terms.is_empty() => {
                    let (e2, p2) = (e.clone(), p.clone());
                    let html = tokio::task::spawn_blocking(move || -> Result<String> {
                        let res = e2.run(&pq, e2.default_options(), Some(&rw), "refined")?;
                        render_results(&e2, &p2, &res, false, false, None)
                    })
                    .await
                    .unwrap_or_else(|j| Err(j.into()));
                    send(html).await;
                }
                other => {
                    if let Ok(Err(err)) = other {
                        tracing::warn!("rewrite skipped: {err:#}");
                    }
                    send(render_results(&e, &p, &res, false, false, None)).await;
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
    pub time: String,
    /// Set on the first turn of each later day, so the margin can show it.
    pub day: Option<String>,
    /// The first line of the prompt, for the turn index.
    pub excerpt: String,
    pub user: String,
    pub assistant: String,
    pub tools: Vec<ToolView>,
    pub tools_summary: String,
    pub tools_err: bool,
    pub matched: bool,
    /// Set when the branch differs from the previous turn's.
    pub branch: Option<String>,
}

pub struct EntityGroup {
    pub name: &'static str,
    /// Label, how the session relates to it, its recherche page and its
    /// GitHub page; either link is empty when there is none.
    pub items: Vec<(String, String, String, String)>,
}

const TASK_ERR_TTL: Duration = Duration::from_secs(60);

#[derive(Template)]
#[template(path = "session_summary.html")]
struct SessionSummaryTpl {
    session_id: String,
    session_enc: String,
    summary: Option<String>,
    summary_note: Option<String>,
    can_summarize: bool,
    summarize_poll: bool,
    summarize_busy: bool,
    summarize_label: &'static str,
    summarize_err: Option<String>,
    oob_title: Option<String>,
}

#[derive(Template)]
#[template(path = "session_reindex.html")]
struct SessionReindexTpl {
    session_id: String,
    session_enc: String,
    reindex_poll: bool,
    reindex_busy: bool,
    reindex_label: &'static str,
    reindex_err: Option<String>,
}

#[derive(Template)]
#[template(path = "session.html")]
struct SessionTpl {
    session_id: String,
    session_enc: String,
    title: String,
    agent: String,
    summary: Option<String>,
    /// Why there is no summary yet, shown in its place.
    summary_note: Option<String>,
    can_summarize: bool,
    summarize_poll: bool,
    summarize_busy: bool,
    summarize_label: &'static str,
    summarize_err: Option<String>,
    oob_title: Option<String>,
    reindex_poll: bool,
    reindex_busy: bool,
    reindex_label: &'static str,
    reindex_err: Option<String>,
    chips: Vec<Chip>,
    cwd: String,
    cwd_gone: bool,
    day: String,
    duration: String,
    started: String,
    ended: String,
    model: String,
    resume: String,
    /// The resume command without its `cd`; the directory is shown below it.
    resume_cmd: String,
    turns: Vec<TurnView>,
    n_matched: usize,
    entities: Vec<EntityGroup>,
    parent: Option<(String, String)>,
    children: Vec<(String, String)>,
    q: String,
    back: String,
}

fn session_title(s: &SessionMeta) -> String {
    if s.title.is_empty() {
        "Untitled session".into()
    } else {
        s.title.clone()
    }
}

fn summary_view(
    e: &Engine,
    cat: &Catalog,
    s: &SessionMeta,
    state: Option<TaskState>,
    include_oob_title: bool,
) -> SessionSummaryTpl {
    let can_summarize = e.llm.summaries_enabled() && crate::summarize::eligible(&e.cfg, s);
    let has_llm_summary = s.summary.is_some();
    let idle_label = if has_llm_summary {
        "Regenerate"
    } else {
        "Summarize"
    };
    let (summarize_poll, summarize_busy, summarize_label, summarize_err) = match state {
        Some(TaskState::Queued) => (true, true, "Queued…", None),
        Some(TaskState::Running(_)) => (true, true, "Summarizing…", None),
        Some(TaskState::Failed(err)) => (false, false, idle_label, Some(err)),
        Some(TaskState::Done(_)) | None => (false, false, idle_label, None),
    };
    SessionSummaryTpl {
        session_id: s.id.clone(),
        session_enc: enc(&s.id),
        summary: s
            .summary
            .as_deref()
            .or(s.recap.as_deref())
            .map(|s| summary_html(s, &[])),
        summary_note: pending(e, cat, s).map(|p| pending_note(p, e.cfg.llm.idle_minutes)),
        can_summarize,
        summarize_poll,
        summarize_busy,
        summarize_label,
        summarize_err,
        oob_title: include_oob_title.then(|| session_title(s)),
    }
}

fn reindex_view(session_id: &str, state: Option<TaskState>) -> SessionReindexTpl {
    let (reindex_poll, reindex_busy, reindex_label, reindex_err) = match state {
        Some(TaskState::Queued) => (true, true, "Queued…", None),
        Some(TaskState::Running(_)) => (true, true, "Re-indexing…", None),
        Some(TaskState::Failed(err)) => (false, false, "Re-index", Some(err)),
        Some(TaskState::Done(_)) | None => (false, false, "Re-index", None),
    };
    SessionReindexTpl {
        session_id: session_id.to_string(),
        session_enc: enc(session_id),
        reindex_poll,
        reindex_busy,
        reindex_label,
        reindex_err,
    }
}

fn all_tasks_view(
    e: &Engine,
    reindex_state: Option<TaskState>,
    summarize_state: Option<TaskState>,
    all_since: u64,
    oob_sessions: Option<usize>,
) -> AllTasksTpl {
    let mut done_msgs = vec![];
    let mut err_msgs = vec![];
    let (reindex_all_busy, reindex_all_label) = match reindex_state {
        Some(TaskState::Queued) => (true, "Queued…"),
        Some(TaskState::Running(_)) => (true, "Re-indexing…"),
        Some(TaskState::Done(msg)) => {
            if let Some(m) = msg {
                done_msgs.push(m);
            }
            (false, "Re-index all")
        }
        Some(TaskState::Failed(err)) => {
            err_msgs.push(err);
            (false, "Re-index all")
        }
        None => (false, "Re-index all"),
    };
    let (summarize_all_busy, summarize_all_label) = match summarize_state {
        Some(TaskState::Queued) => (true, "Queued…".into()),
        Some(TaskState::Running(Some(p))) => (true, format!("Summarizing {p}…")),
        Some(TaskState::Running(None)) => (true, "Summarizing…".into()),
        Some(TaskState::Done(msg)) => {
            if let Some(m) = msg {
                done_msgs.push(m);
            }
            (false, "Summarize all".into())
        }
        Some(TaskState::Failed(err)) => {
            err_msgs.push(err);
            (false, "Summarize all".into())
        }
        None => (false, "Summarize all".into()),
    };
    AllTasksTpl {
        summaries_enabled: e.llm.summaries_enabled(),
        all_poll: reindex_all_busy || summarize_all_busy,
        all_since,
        reindex_all_busy,
        reindex_all_label,
        summarize_all_busy,
        summarize_all_label,
        all_done: (!done_msgs.is_empty()).then(|| done_msgs.join(" · ")),
        all_err: (!err_msgs.is_empty()).then(|| err_msgs.join(" · ")),
        oob_sessions,
    }
}

#[derive(Deserialize)]
struct AllTasksQuery {
    #[serde(default)]
    since: u64,
}

async fn all_tasks_status(
    State(e): State<AppState>,
    Query(q): Query<AllTasksQuery>,
) -> Result<Response, AppError> {
    let cur_seq = e.tasks.completed_seq();
    let updated = cur_seq > q.since;
    let reindex_state = e.tasks.poll_state(&Task::ReindexAll, q.since);
    let summarize_state = e.tasks.poll_state(&Task::SummarizeAll, q.since);
    let oob_sessions = updated.then(|| e.catalog().sessions.len());
    let html =
        all_tasks_view(&e, reindex_state, summarize_state, cur_seq, oob_sessions).render()?;
    let mut resp = no_store_html(html);
    if updated {
        resp.headers_mut()
            .insert("HX-Trigger", HeaderValue::from_static("tasks-updated"));
    }
    Ok(resp)
}

async fn reindex_all_start(
    State(e): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = check_same_origin(&headers) {
        return Ok(err.into_response());
    }
    let since = e.tasks.completed_seq();
    let reindex_state = e.tasks.enqueue(Task::ReindexAll);
    let summarize_state = e.tasks.poll_state(&Task::SummarizeAll, since);
    let html = all_tasks_view(&e, Some(reindex_state), summarize_state, since, None).render()?;
    Ok(no_store_html(html))
}

async fn summarize_all_start(
    State(e): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = check_same_origin(&headers) {
        return Ok(err.into_response());
    }
    if !e.llm.summaries_enabled() {
        return Ok((StatusCode::BAD_REQUEST, "Summaries are disabled.").into_response());
    }
    let since = e.tasks.completed_seq();
    let summarize_state = e.tasks.enqueue(Task::SummarizeAll);
    let reindex_state = e.tasks.poll_state(&Task::ReindexAll, since);
    let html = all_tasks_view(&e, reindex_state, Some(summarize_state), since, None).render()?;
    Ok(no_store_html(html))
}

async fn summarize_all_cancel(
    State(e): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = check_same_origin(&headers) {
        return Ok(err.into_response());
    }
    let before = e.tasks.completed_seq();
    let summarize_state = e.tasks.cancel(&Task::SummarizeAll);
    let after = e.tasks.completed_seq();
    let reindex_state = e.tasks.poll_state(&Task::ReindexAll, before);
    let html = all_tasks_view(&e, reindex_state, summarize_state, after, None).render()?;
    Ok(no_store_html(html))
}

fn check_same_origin(headers: &HeaderMap) -> Option<(StatusCode, &'static str)> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && !matches!(site, "same-origin" | "none")
    {
        return Some((
            StatusCode::FORBIDDEN,
            "Cross-site requests are not allowed.",
        ));
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        let origin_host = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .map(|rest| rest.split('/').next().unwrap_or(rest));
        if host.is_none() || origin_host != host {
            return Some((StatusCode::FORBIDDEN, "Origin mismatch."));
        }
    }
    None
}

fn no_store_html(html: String) -> Response {
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Deserialize)]
struct TaskSession {
    session: String,
}

async fn summarize_start(
    State(e): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TaskSession>,
) -> Result<Response, AppError> {
    if let Some(err) = check_same_origin(&headers) {
        return Ok(err.into_response());
    }
    let cat = e.catalog();
    let Some(s) = cat.sessions.get(&form.session) else {
        return Ok((StatusCode::NOT_FOUND, "No session with that ID.").into_response());
    };
    if !e.llm.summaries_enabled() || !crate::summarize::eligible(&e.cfg, s) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "Summaries are unavailable for this session.",
        )
            .into_response());
    }
    let state = e.tasks.enqueue(Task::summarize(&s.id));
    let html = summary_view(&e, &cat, s, Some(state), false).render()?;
    Ok(no_store_html(html))
}

async fn summarize_status(
    State(e): State<AppState>,
    Query(q): Query<TaskSession>,
) -> Result<Response, AppError> {
    let cat = e.catalog();
    let Some(s) = cat.sessions.get(&q.session) else {
        return Ok((StatusCode::NOT_FOUND, "No session with that ID.").into_response());
    };
    let state = e.tasks.state(&Task::summarize(&s.id));
    let done = matches!(state, Some(TaskState::Done(_)));
    let html = summary_view(&e, &cat, s, state, done).render()?;
    Ok(no_store_html(html))
}

async fn reindex_start(
    State(e): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TaskSession>,
) -> Result<Response, AppError> {
    if let Some(err) = check_same_origin(&headers) {
        return Ok(err.into_response());
    }
    let cat = e.catalog();
    let Some(s) = cat.sessions.get(&form.session) else {
        return Ok((StatusCode::NOT_FOUND, "No session with that ID.").into_response());
    };
    let state = e.tasks.enqueue(Task::reindex(&s.id));
    let html = reindex_view(&s.id, Some(state)).render()?;
    Ok(no_store_html(html))
}

async fn reindex_status(
    State(e): State<AppState>,
    Query(q): Query<TaskSession>,
) -> Result<Response, AppError> {
    let cat = e.catalog();
    let state = e.tasks.state(&Task::reindex(&q.session));
    if !cat.sessions.contains_key(&q.session) && state.is_none() {
        return Ok((StatusCode::NOT_FOUND, "No session with that ID.").into_response());
    }
    let done = matches!(state, Some(TaskState::Done(_)));
    let html = reindex_view(&q.session, state).render()?;
    let mut resp = no_store_html(html);
    if done {
        resp.headers_mut()
            .insert("HX-Refresh", HeaderValue::from_static("true"));
    }
    Ok(resp)
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
        let mut turns: Vec<TurnView> = vec![];
        let mut last_day = fmt_day(s.started);
        let mut last_branch: Option<String> = None;
        for t in crate::store::turns(&c, &id)? {
            let user_text = shown_prompt(&e, &t.user_text);
            let (user, uh) = markdown(&user_text, &terms);
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
            let label = if !user_text.trim().is_empty() {
                user_text.as_ref()
            } else {
                t.assistant_text.as_str()
            };
            let day = t.ts.map(fmt_day).filter(|d| *d != last_day);
            if let Some(d) = &day {
                last_day = d.clone();
            }
            let branch = t.branch.filter(|b| last_branch.as_ref() != Some(b));
            if branch.is_some() {
                last_branch = branch.clone();
            }
            let mut tool_names: Vec<&str> = vec![];
            for tv in &tools {
                if !tv.name.is_empty() && !tool_names.contains(&tv.name.as_str()) {
                    tool_names.push(&tv.name);
                    if tool_names.len() == 4 {
                        break;
                    }
                }
            }
            let tools_summary = tool_names.join(", ");
            let tools_err = tools.iter().any(|tv| tv.error);
            turns.push(TurnView {
                idx: t.idx,
                when: t.ts.map(fmt_date).unwrap_or_default(),
                time: t.ts.map(fmt_time).unwrap_or_default(),
                day,
                excerpt: crate::model::clip_chars(
                    label.lines().find(|l| !l.trim().is_empty()).unwrap_or(""),
                    90,
                ),
                user,
                assistant,
                tools,
                tools_summary,
                tools_err,
                matched: uh + ah > 0,
                branch,
            });
        }
        let mut groups: BTreeMap<&str, Vec<(String, String, String, String)>> = BTreeMap::new();
        for en in cat.entities.get(&id).into_iter().flatten() {
            let name = match en.kind.as_str() {
                "pr" => "Pull requests",
                "branch" => "Branches",
                "ticket" => "Tickets",
                "commit" => "Commits",
                "worktree" => "Directories",
                "repo" => "Repos mentioned",
                // File entities are still extracted for search, but listing them here is mostly noise.
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
                "worktree" | "commit" => String::new(),
                k => format!("/e?kind={k}&key={}", enc(&en.key.to_lowercase())),
            };
            let github = github_url(&en.kind, &en.key, s.repo_key.as_deref()).unwrap_or_default();
            groups
                .entry(name)
                .or_default()
                .push((en.display.clone(), how, href, github));
        }
        let order = [
            "Pull requests",
            "Tickets",
            "Branches",
            "Commits",
            "Directories",
            "Repos mentioned",
        ];
        let entities = order
            .iter()
            .filter_map(|n| {
                groups.remove(n).map(|mut items| {
                    items.truncate(15);
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
        let sum = summary_view(
            &e,
            &cat,
            s,
            e.tasks.page_state(&Task::summarize(&s.id), TASK_ERR_TTL),
            false,
        );
        let reidx = reindex_view(
            &s.id,
            e.tasks.page_state(&Task::reindex(&s.id), TASK_ERR_TTL),
        );
        let tpl = SessionTpl {
            session_id: sum.session_id,
            session_enc: sum.session_enc,
            title: session_title(s),
            agent: s.source.clone(),
            summary: sum.summary,
            summary_note: sum.summary_note,
            can_summarize: sum.can_summarize,
            summarize_poll: sum.summarize_poll,
            summarize_busy: sum.summarize_busy,
            summarize_label: sum.summarize_label,
            summarize_err: sum.summarize_err,
            oob_title: None,
            reindex_poll: reidx.reindex_poll,
            reindex_busy: reidx.reindex_busy,
            reindex_label: reidx.reindex_label,
            reindex_err: reidx.reindex_err,
            chips: chips_for(&cat, s),
            cwd: s.cwd.clone().unwrap_or_default(),
            cwd_gone: s
                .cwd
                .as_ref()
                .is_some_and(|c| !std::path::Path::new(c).exists()),
            day: fmt_day(s.started),
            duration: fmt_duration(s.ended - s.started),
            started: fmt_date(s.started),
            ended: fmt_date(s.ended),
            model: s.model.clone().unwrap_or_default(),
            resume: crate::resume_command(s),
            resume_cmd: crate::agent_resume(s),
            n_matched: turns.iter().filter(|t| t.matched).count(),
            turns,
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
    count: String,
    /// Colours the heading like the ref it came from.
    ref_class: String,
    github: Option<String>,
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
        let ids = cat.entity_sessions(&ep.kind, &ep.key);
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
        // Matches the hrefs `chips_for` gives each ref.
        let key = if ep.kind == "ticket" {
            ep.key.to_lowercase()
        } else {
            ep.key.clone()
        };
        let own = format!("/e?kind={}&key={}", ep.kind, enc(&key));
        let mut results_html = render_results(&e, &p, &res, false, false, Some(&own))?;
        if ids.is_empty() {
            results_html = "<p class=\"empty\">No sessions reference this.</p>".into();
        }
        let kind = match ep.kind.as_str() {
            "repo" => "Repository",
            "pr" => "Pull request",
            "ticket" => "Ticket",
            "branch" => "Branch",
            _ => "Entity",
        };
        let n = ids.len();
        Ok(EntityTpl {
            github: github_url(&ep.kind, &ep.key, None),
            heading: display,
            kind: kind.into(),
            count: format!("{n} {}", if n == 1 { "session" } else { "sessions" }),
            ref_class: match ep.kind.as_str() {
                "repo" | "branch" | "pr" | "ticket" => ep.kind.clone(),
                _ => String::new(),
            },
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

/// The thumbs up / down on a result add it to the eval set. The page shows
/// the vote itself, so there is nothing to send back.
async fn feedback(
    State(e): State<AppState>,
    Form(f): Form<FeedbackForm>,
) -> Result<StatusCode, AppError> {
    let label = if f.label == "good" { "good" } else { "bad" };
    let c = e.conn()?;
    c.execute(
        "INSERT INTO feedback (ts, query, session_id, label) VALUES (?, ?, ?, ?)",
        rusqlite::params![chrono::Utc::now().timestamp(), f.q, f.session, label],
    )?;
    Ok(StatusCode::NO_CONTENT)
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

    #[tokio::test]
    async fn search_streams_even_without_keyword_matches() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.llm.enabled = false;
        let engine = Arc::new(Engine::open_in(cfg.clone(), &dir.path().join("vectors")).unwrap());
        let response = router(engine)
            .oneshot(
                Request::get("/?q=unmatched+query")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            body.contains("sse-connect=\"/stream?q=unmatched+query\""),
            "{body}"
        );
        assert!(!body.contains("No session matches"), "{body}");

        cfg.search.vectors = false;
        cfg.search.rerank = false;
        let engine = Arc::new(Engine::open_in(cfg, &dir.path().join("keyword")).unwrap());
        let response = router(engine)
            .oneshot(
                Request::get("/?q=unmatched+query")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("No session matches"), "{body}");
        assert!(!body.contains("Ranking by meaning"), "{body}");
        assert!(!body.contains("sse-connect"), "{body}");
    }

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
    fn system_instructions_hide_only_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.llm.enabled = false;
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        let mut engine = Engine::open_in(cfg, dir.path()).unwrap();
        let text = "<system_instruction>\nbe brief\n</system_instruction>\n\n<system_instruction>x</system_instruction>\nfix the test";
        assert_eq!(shown_prompt(&engine, text), text);
        engine.cfg.ui.hide_system_instructions = true;
        assert_eq!(shown_prompt(&engine, text), "fix the test");
    }

    #[test]
    fn excerpts_drop_markdown_noise() {
        let text = "### Plan\nKeep **walking**, see [the notes](</tmp/a b.md>) and `cargo test`.";
        let html = code_spans(&highlight(&plain_excerpt(text), &[], None).0);
        assert_eq!(
            html,
            "Plan\nKeep walking, see the notes and <code>cargo test</code>."
        );
        // Clipped inside a span at either end: the cut span stays code.
        let mid = code_spans(&highlight(&plain_excerpt("ab `cd ef` gh `ij kl`"), &[], None).0);
        assert_eq!(mid, "ab <code>cd ef</code> gh <code>ij kl</code>");
        let cut = format!("cd{CODE_CLOSE} gh {CODE_OPEN}ij");
        assert_eq!(code_spans(&cut), "<code>cd</code> gh <code>ij</code>");
    }

    #[test]
    fn blurb_keeps_whole_bullets() {
        let html = blurb_html("- one two\n- three four\n- five six", &[], 22);
        assert_eq!(html, "<ul><li>one two</li><li>three four</li></ul>");
        let html = blurb_html("- a very long first bullet", &[], 8);
        assert!(html.starts_with("<ul><li>a very…"), "{html}");
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
                session_id: "summary-example".into(),
                session_enc: "summary-example".into(),
                title: "Summary example".into(),
                agent: "claude".into(),
                summary: Some(summary_html(summary, &[])),
                summary_note: None,
                can_summarize: false,
                summarize_poll: false,
                summarize_busy: false,
                summarize_label: "Regenerate",
                summarize_err: None,
                oob_title: None,
                reindex_poll: false,
                reindex_busy: false,
                reindex_label: "Re-index",
                reindex_err: None,
                chips: vec![],
                cwd: String::new(),
                cwd_gone: false,
                day: String::new(),
                duration: String::new(),
                started: String::new(),
                ended: String::new(),
                model: String::new(),
                resume: String::new(),
                resume_cmd: String::new(),
                turns: vec![],
                n_matched: 0,
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
    fn match_reasons_land_on_the_ref_they_matched() {
        let chip = |class, label: &str| Chip {
            label: label.into(),
            href: String::new(),
            class,
            title: String::new(),
            github: None,
            hit: false,
            line: 0,
        };
        let mut chips = vec![
            chip("chip repo", "widgets"),
            chip("chip branch", "fix-order"),
            chip("chip pr", "PR #12"),
        ];
        let reasons: Vec<String> = [
            "exact pr acme/widgets#12",
            "semantic",
            "branch fix-order",
            "time",
            "exact commit abc123",
            "rewrite",
        ]
        .map(String::from)
        .into();
        let (time_hit, notes) = place_reasons(&reasons, &mut chips);
        assert!(time_hit);
        assert_eq!(
            chips.iter().map(|c| c.hit).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(notes, ["similar meaning", "commit abc123", "related terms"]);

        // Matched words are already highlighted, so meaning needs no note.
        let (_, notes) = place_reasons(&["keyword".into(), "semantic".into()], &mut chips);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn github_links_only_for_github_entities() {
        let repo = Some("github.com/acme/widgets");
        assert_eq!(
            github_url("pr", "acme/widgets#42", None).as_deref(),
            Some("https://github.com/acme/widgets/pull/42")
        );
        assert_eq!(
            github_url("repo", "github.com/acme/widgets", None).as_deref(),
            Some("https://github.com/acme/widgets")
        );
        // Slashes stay path separators; `#` would otherwise start a fragment.
        assert_eq!(
            github_url("branch", "feature/Fix#2", repo).as_deref(),
            Some("https://github.com/acme/widgets/tree/feature/Fix%232")
        );
        for (kind, key, repo) in [
            ("pr", "gitlab.com/acme/widgets#42", None),
            ("repo", "gitlab.com/acme/widgets", None),
            ("repo", "path:widgets", None),
            ("branch", "fix-order", None),
            ("branch", "fix-order", Some("path:widgets")),
            ("branch", "fix-order", Some("gitlab.com/acme/widgets")),
            ("commit", "abc1234", repo),
        ] {
            assert_eq!(github_url(kind, key, repo), None, "{kind} {key} {repo:?}");
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

    #[tokio::test]
    async fn session_page_on_demand_summarize_and_reindex_queue_flow() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use axum::routing::post as ax_post;
        use tower::ServiceExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = listener.local_addr().unwrap();
        let llm_app = Router::new().route(
            "/chat/completions",
            ax_post(|| async {
                axum::Json(serde_json::json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {
                            "content": serde_json::json!({
                                "title": "Fresh generated title",
                                "summary": "- Generated from web UI.",
                                "kind": "feature",
                                "outcome": "shipped",
                                "tags": ["ui"]
                            }).to_string()
                        }
                    }],
                    "usage": {"completion_tokens": 20}
                }))
            }),
        );
        let llm_server = tokio::spawn(async move {
            let _ = axum::serve(listener, llm_app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("claude/projects/repo");
        std::fs::create_dir_all(&project).unwrap();
        let line = |sid: &str, agent: Option<&str>, text: &str| {
            let mut v = serde_json::json!({
                "type": "user", "sessionId": sid, "cwd": "/work/repo",
                "timestamp": "2026-01-01T00:00:00Z", "message": {"role": "user", "content": text},
            });
            if let Some(a) = agent {
                v["agentId"] = a.into();
            }
            v.to_string()
        };
        let main_path = project.join("s1.jsonl");
        std::fs::write(
            &main_path,
            format!("{}\n", line("s1", None, "first prompt")),
        )
        .unwrap();
        let sub_dir = project.join("s1/subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(
            sub_dir.join("agent-a1.jsonl"),
            format!("{}\n", line("s1", Some("a1"), "subagent prompt")),
        )
        .unwrap();

        let mut cfg = crate::config::Config::default();
        cfg.sources.claude.dir = Some(dir.path().join("claude"));
        cfg.sources.codex.dir = Some(dir.path().join("codex"));
        cfg.sources.antigravity.dir = Some(dir.path().join("antigravity"));
        cfg.sources.cursor.dir = Some(dir.path().join("cursor"));
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        cfg.llm.enabled = true;
        cfg.llm.summaries = true;
        cfg.llm.api_key_env = "HOME".into();
        cfg.llm.base_url = format!("http://{llm_addr}");
        let engine = Arc::new(Engine::open_in(cfg, &dir.path().join("data")).unwrap());
        engine.refresh_with(false, false).unwrap();
        let app = router(engine.clone());

        let send = |req: Request<Body>| {
            let app = app.clone();
            async move {
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let headers = resp.headers().clone();
                let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
            }
        };

        // Session page shows both Re-index and Summarize for top-level sessions.
        let (status, _, body) = send(Request::get("/s/s1").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Re-index"), "{body}");
        assert!(body.contains(">Summarize</button>"), "{body}");

        // Subagent page shows Re-index (with slash in ID) but not Summarize.
        let (status, _, body) =
            send(Request::get("/s/s1/agent-a1").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Re-index"), "{body}");
        assert!(!body.contains("/session/summarize"), "{body}");

        // Cross-site POSTs are rejected.
        let (status, _, _) = send(
            Request::post("/session/summarize")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", "cross-site")
                .body(Body::from("session=s1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = send(
            Request::post("/session/reindex")
                .header("content-type", "application/x-www-form-urlencoded")
                .header(header::HOST, "127.0.0.1:7777")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::from("session=s1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Subagent summarize request is rejected.
        let (status, _, _) = send(
            Request::post("/session/summarize")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("session=s1%2Fagent-a1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Enqueue summarize via POST -> queued polling fragment.
        let (status, _, body) = send(
            Request::post("/session/summarize")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("sec-fetch-site", "same-origin")
                .header(header::HOST, "127.0.0.1:7777")
                .header(header::ORIGIN, "http://127.0.0.1:7777")
                .body(Body::from("session=s1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Queued…"), "{body}");
        assert!(
            body.contains("hx-get=\"/session/summarize?session=s1\""),
            "{body}"
        );

        // Drain queue on a blocking thread just as daemon::run does.
        let e2 = engine.clone();
        let ran = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();
        assert_eq!(ran, 1);

        // Status poll returns the new summary, Regenerate button, no polling, and OOB title.
        let (status, _, body) = send(
            Request::get("/session/summarize?session=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Generated from web UI."), "{body}");
        assert!(body.contains(">Regenerate</button>"), "{body}");
        assert!(!body.contains("hx-trigger="), "{body}");
        assert!(
            body.contains(
                "<h1 id=\"session-title\" hx-swap-oob=\"true\">Fresh generated title</h1>"
            ),
            "{body}"
        );

        // Grow session file, enqueue reindex, drain, and check HX-Refresh.
        std::fs::write(
            &main_path,
            format!(
                "{}\n{}\n",
                line("s1", None, "first prompt"),
                line("s1", None, "second prompt")
            ),
        )
        .unwrap();
        let (status, _, body) = send(
            Request::post("/session/reindex")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("session=s1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Queued…"), "{body}");
        assert!(
            body.contains("hx-get=\"/session/reindex?session=s1\""),
            "{body}"
        );

        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();

        let (status, headers, body) = send(
            Request::get("/session/reindex?session=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("HX-Refresh").and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert!(!body.contains("hx-trigger="), "{body}");
        assert_eq!(engine.catalog().sessions["s1"].turn_count, 2);

        // Subagent reindex with slash works end-to-end.
        let (status, _, _) = send(
            Request::post("/session/reindex")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("session=s1%2Fagent-a1"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Failing LLM call surfaces error in the summary fragment.
        llm_server.abort();
        let _ = send(
            Request::post("/session/summarize")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("session=s1"))
                .unwrap(),
        )
        .await;
        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();
        let (status, _, body) = send(
            Request::get("/session/summarize?session=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("class=\"task-err\""), "{body}");
        assert!(!body.contains("hx-trigger="), "{body}");
    }

    #[tokio::test]
    async fn homepage_reindex_all_and_summarize_all_queue_flow() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use axum::routing::post as ax_post;
        use tower::ServiceExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llm_addr = listener.local_addr().unwrap();
        let llm_app = Router::new().route(
            "/chat/completions",
            ax_post(|| async {
                axum::Json(serde_json::json!({
                    "choices": [{
                        "finish_reason": "stop",
                        "message": {
                            "content": serde_json::json!({
                                "title": "Batch summarized title",
                                "summary": "- Summarized via Summarize all.",
                                "kind": "feature",
                                "outcome": "shipped",
                                "tags": ["batch"]
                            }).to_string()
                        }
                    }],
                    "usage": {"completion_tokens": 20}
                }))
            }),
        );
        let llm_server = tokio::spawn(async move {
            let _ = axum::serve(listener, llm_app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("claude/projects/repo");
        std::fs::create_dir_all(&project).unwrap();
        let line = |sid: &str, text: &str| {
            serde_json::json!({
                "type": "user", "sessionId": sid, "cwd": "/work/repo",
                "timestamp": "2026-01-01T00:00:00Z", "message": {"role": "user", "content": text},
            })
            .to_string()
        };
        std::fs::write(project.join("s1.jsonl"), format!("{}\n", line("s1", "one"))).unwrap();

        let mut cfg = crate::config::Config::default();
        cfg.sources.claude.dir = Some(dir.path().join("claude"));
        cfg.sources.codex.dir = Some(dir.path().join("codex"));
        cfg.sources.antigravity.dir = Some(dir.path().join("antigravity"));
        cfg.sources.cursor.dir = Some(dir.path().join("cursor"));
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        cfg.llm.enabled = true;
        cfg.llm.summaries = true;
        cfg.llm.api_key_env = "HOME".into();
        cfg.llm.base_url = format!("http://{llm_addr}");
        let engine = Arc::new(Engine::open_in(cfg, &dir.path().join("data")).unwrap());
        engine.refresh_with(false, false).unwrap();
        let app = router(engine.clone());

        let send = |req: Request<Body>| {
            let app = app.clone();
            async move {
                let resp = app.oneshot(req).await.unwrap();
                let status = resp.status();
                let headers = resp.headers().clone();
                let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
            }
        };

        // Homepage renders both Re-index all and Summarize all buttons.
        let (status, _, body) = send(Request::get("/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(">Re-index all</button>"), "{body}");
        assert!(body.contains(">Summarize all</button>"), "{body}");

        // Cross-site POSTs are rejected.
        for path in [
            "/tasks/reindex",
            "/tasks/summarize",
            "/tasks/summarize/cancel",
        ] {
            let (status, _, _) = send(
                Request::post(path)
                    .header("sec-fetch-site", "cross-site")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }

        // Add a second session on disk, click Re-index all, drain, and verify poll response.
        std::fs::write(project.join("s2.jsonl"), format!("{}\n", line("s2", "two"))).unwrap();
        let (status, _, body) = send(
            Request::post("/tasks/reindex")
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Queued…"), "{body}");
        assert!(body.contains("hx-get=\"/tasks/all?since=0\""), "{body}");

        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();

        let (status, headers, body) = send(
            Request::get("/tasks/all?since=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("HX-Trigger").and_then(|v| v.to_str().ok()),
            Some("tasks-updated")
        );
        assert!(body.contains("Re-indexed 2 sessions"), "{body}");
        assert!(
            body.contains("<span id=\"session-count\" hx-swap-oob=\"true\">2</span>"),
            "{body}"
        );
        assert!(!body.contains("hx-trigger="), "{body}");

        // Cancel a queued Summarize all immediately.
        let (status, _, body) = send(
            Request::post("/tasks/summarize")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(">Cancel</button>"), "{body}");
        let (status, _, body) = send(
            Request::post("/tasks/summarize/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Cancelled"), "{body}");
        assert!(!body.contains("hx-trigger="), "{body}");

        // Enqueue Summarize all, drain, and check summaries + completion note.
        let seq = engine.tasks.completed_seq();
        let (status, _, _) = send(
            Request::post("/tasks/summarize")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();

        let (status, headers, body) = send(
            Request::get(format!("/tasks/all?since={seq}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("HX-Trigger").and_then(|v| v.to_str().ok()),
            Some("tasks-updated")
        );
        assert!(body.contains("Summarized 2 sessions"), "{body}");
        let cat = engine.catalog();
        assert_eq!(cat.sessions["s1"].title, "Batch summarized title");
        assert_eq!(cat.sessions["s2"].title, "Batch summarized title");

        // Second Summarize all sees nothing left to summarize.
        let seq = engine.tasks.completed_seq();
        let _ = send(
            Request::post("/tasks/summarize")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();
        let (status, _, body) = send(
            Request::get(format!("/tasks/all?since={seq}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("All sessions up to date"), "{body}");

        // Failing Summarize all surfaces the API error.
        llm_server.abort();
        engine
            .conn()
            .unwrap()
            .execute(
                "UPDATE sessions SET summary = NULL, summary_turns = NULL WHERE id = 's1'",
                [],
            )
            .unwrap();
        let seq = engine.tasks.completed_seq();
        let _ = send(
            Request::post("/tasks/summarize")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let e2 = engine.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            crate::daemon::drain_tasks(&e2, &rt)
        })
        .await
        .unwrap();
        let (status, _, body) = send(
            Request::get(format!("/tasks/all?since={seq}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("class=\"task-err-inline\""), "{body}");
    }
}
