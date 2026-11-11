//! Reads a query before searching: strips conversational filler, pulls out
//! filters, exact entities (tickets, PRs, SHAs, paths, branches), time
//! phrases and intent words such as "fixed" or "discussed".

use crate::store::Catalog;
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub repo: Option<String>,
    pub agent: Option<String>,
    pub branch: Option<String>,
    pub kind: Option<String>,
    pub outcome: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.repo.is_none()
            && self.agent.is_none()
            && self.branch.is_none()
            && self.kind.is_none()
            && self.outcome.is_none()
            && self.since.is_none()
            && self.until.is_none()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Exact {
    pub kind: String,
    pub label: String,
    /// The entity keys that matched, so each session's link strength is
    /// taken from the matching entity.
    pub keys: Vec<String>,
    pub sessions: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub raw: String,
    /// The query without filler and operators: what BM25 and embeddings see.
    pub text: String,
    pub phrases: Vec<String>,
    pub filters: Filters,
    pub exact: Vec<Exact>,
    pub repo_boost: Vec<String>,
    /// Sessions whose branch name shares most of its words with the query.
    pub branch_boost: Vec<(String, String)>,
    pub time_boost: Option<(i64, i64)>,
    pub kinds: Vec<&'static str>,
    pub outcomes: Vec<&'static str>,
    /// Reads like a sentence and names nothing exact: a rewrite may help.
    pub sentence: bool,
}

static FILLER: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^(please\s+)?(can you\s+|could you\s+)?(find|show|get|search|look up|where is|where's|what was)(\s+me)?\s+",
        r"^(the|that|a|an|some)\s+",
        r"^(thread|session|conversation|chat|time|one)s?\s+",
        r"^(where|in which|when|that|in that|wherein)\s+",
        r"^(we|i|you|claude|codex)\s+",
        r"^(were|was|have|had|did)\s+",
        r"^(talked about|talking about|worked on|working on|looked at|looking at|about|on)\s+",
    ]
    .iter()
    .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
    .collect()
});

static OPERATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b(repo|agent|branch|kind|outcome|since|until):("[^"]+"|\S+)"#).unwrap()
});
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
static PR_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"github\.com/([\w.-]+)/([\w.-]+)/pull/(\d+)").unwrap());
static PR_NUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:\bpr\s*#?|#)(\d{1,6})\b").unwrap());
static SHA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,40}\b").unwrap());
static PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[\w.-]+/)+[\w.-]+|\b[\w-]+\.(?:rs|go|py|ts|tsx|js|jsx|json|toml|ya?ml|md|sql|sh|swift|kt|java|rb|css|html|lock)\b").unwrap()
});
static IN_REPO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:in|from|for)\s+(?:the\s+)?([\w.-]+)(?:\s+(?:repo|repository|project))?")
        .unwrap()
});

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

pub fn parse(raw: &str, cat: &Catalog, tickets: &[Regex], stoplist: &[String]) -> Parsed {
    let mut p = Parsed {
        raw: raw.trim().to_string(),
        ..Default::default()
    };
    let mut text = raw.trim().to_string();

    for c in OPERATOR.captures_iter(raw) {
        let v = c[2].trim_matches('"').to_string();
        match &c[1] {
            "repo" => p.filters.repo = resolve_repo(cat, &v).or(Some(v)),
            "agent" => p.filters.agent = Some(v.to_lowercase()),
            "branch" => p.filters.branch = Some(v),
            "kind" => p.filters.kind = Some(v.to_lowercase()),
            "outcome" => p.filters.outcome = Some(v.to_lowercase()),
            "since" => p.filters.since = parse_date(&v),
            "until" => p.filters.until = parse_date(&v).map(|t| t + 86_400),
            _ => {}
        }
    }
    text = OPERATOR.replace_all(&text, " ").into_owned();
    p.phrases = QUOTED
        .captures_iter(&text)
        .map(|c| c[1].to_string())
        .collect();

    // Repo names first, so a bare "#16" can prefer the repo the query names.
    for w in text.split_whitespace() {
        if let Some(k) = resolve_repo(
            cat,
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.'),
        ) && !p.repo_boost.contains(&k)
        {
            p.repo_boost.push(k);
        }
    }

    // Exact entities. These rank first when they match a session.
    let find = |kind: &str, pred: &dyn Fn(&str) -> bool| -> (Vec<String>, Vec<String>) {
        let mut keys = vec![];
        let mut sessions = vec![];
        for ((k, key), v) in &cat.by_entity {
            if k == kind && pred(key) {
                keys.push(key.clone());
                sessions.extend(v.iter().cloned());
            }
        }
        sessions.sort();
        sessions.dedup();
        (keys, sessions)
    };
    let mut exact = |kind: &str, label: String, (keys, sessions): (Vec<String>, Vec<String>)| {
        if !sessions.is_empty() && !p.exact.iter().any(|e| e.kind == kind && e.label == label) {
            p.exact.push(Exact {
                kind: kind.to_string(),
                label,
                keys,
                sessions,
            });
        }
    };
    for re in tickets {
        for m in re.find_iter(&text.to_uppercase()) {
            let t = m.as_str().to_lowercase();
            if !stoplist
                .iter()
                .any(|s| t.starts_with(&format!("{}-", s.to_lowercase())))
            {
                exact("ticket", t.to_uppercase(), find("ticket", &|k| k == t));
            }
        }
    }
    for c in PR_URL.captures_iter(&text) {
        let key = format!("{}/{}#{}", &c[1], &c[2], &c[3]).to_lowercase();
        exact("pr", key.clone(), find("pr", &|k| k == key));
    }
    for c in PR_NUM.captures_iter(&text) {
        let suffix = format!("#{}", &c[1]);
        let in_repo = |k: &str| {
            p.repo_boost.iter().any(|r| {
                r.ends_with(&format!("/{}", k.split('#').next().unwrap_or("")))
                    || r.ends_with(k.split('#').next().unwrap_or(""))
            })
        };
        let all = find("pr", &|k| k.ends_with(&suffix));
        let preferred = find("pr", &|k| k.ends_with(&suffix) && in_repo(k));
        exact(
            "pr",
            format!("PR {suffix}"),
            if preferred.1.is_empty() {
                all
            } else {
                preferred
            },
        );
    }
    for m in SHA.find_iter(&text) {
        let s = m.as_str();
        if s.chars().any(|c| c.is_ascii_digit()) && s.chars().any(|c| c.is_ascii_alphabetic()) {
            let (mut k1, mut s1) = find("commit", &|k| k.starts_with(s) || s.starts_with(k));
            let (k2, s2) = find("sha", &|k| k.starts_with(s));
            k1.extend(k2);
            s1.extend(s2);
            exact("commit", s.to_string(), (k1, s1));
        }
    }
    for m in PATH.find_iter(&text) {
        let s = m.as_str().to_lowercase();
        if s.len() >= 4 && !s.starts_with("http") && !s.contains("github.com") {
            exact("file", s.clone(), find("file", &|k| k.ends_with(&s)));
        }
    }
    for w in text.split_whitespace() {
        let w = w
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '-' && c != '_')
            .to_lowercase();
        if w.len() >= 4 && (w.contains('-') || w.contains('/') || w.contains('_')) {
            exact("branch", w.clone(), find("branch", &|k| k == w));
        }
    }

    // Branch names describe the work. A branch that shares most of its
    // words with the query is a strong hint, though not an exact match.
    let qwords: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(stem)
        .collect();
    for ((k, key), sessions) in &cat.by_entity {
        if k != "branch" {
            continue;
        }
        let bwords: Vec<String> = key
            .rsplit('/')
            .next()
            .unwrap_or(key)
            .split(['-', '_'])
            .filter(|w| w.len() >= 2)
            .map(stem)
            .collect();
        let shared = bwords.iter().filter(|w| qwords.contains(w)).count();
        if bwords.len() >= 2 && shared >= 2 && shared * 10 >= bwords.len() * 6 {
            for sid in sessions {
                p.branch_boost.push((sid.clone(), key.clone()));
            }
        }
    }

    // "in <repo>" is a filter; a bare repo name is a boost.
    if p.filters.repo.is_none() {
        for c in IN_REPO.captures_iter(&text) {
            if let Some(k) = resolve_repo(cat, &c[1]) {
                p.filters.repo = Some(k);
                text = text.replace(&c[0], " ");
                break;
            }
        }
    }
    let (t, range) = time_phrase(&text);
    text = t;
    p.time_boost = range;

    let low = text.to_lowercase();
    let has = |words: &[&str]| {
        words
            .iter()
            .any(|w| low.split(|c: char| !c.is_alphanumeric()).any(|x| x == *w))
    };
    if has(&[
        "fixed", "fix", "fixing", "solved", "resolved", "debugged", "bug",
    ]) {
        p.kinds.push("bugfix");
        p.outcomes.push("fixed");
    }
    if has(&[
        "discussed",
        "talked",
        "planned",
        "plan",
        "designed",
        "design",
        "brainstormed",
        "brainstorm",
        "decided",
    ]) {
        p.kinds.extend(["design", "research"]);
        p.outcomes.push("planned");
    }
    if has(&["migrated", "migration", "migrate"]) {
        p.kinds.push("migration");
    }
    if has(&["reviewed", "review"]) {
        p.kinds.push("review");
    }
    if has(&["shipped", "released", "merged", "deployed"]) {
        p.outcomes.push("shipped");
    }
    if has(&["investigated", "investigate", "why"]) {
        p.outcomes.push("investigated");
    }

    p.text = strip_filler(&text);
    let words = p.text.split_whitespace().count();
    p.sentence = p.exact.is_empty()
        && p.phrases.is_empty()
        && raw.split_whitespace().count() >= 4
        && words >= 2;
    p
}

pub fn strip_filler(text: &str) -> String {
    let mut t = text.trim().to_string();
    loop {
        let before = t.clone();
        for re in FILLER.iter() {
            t = re.replace(&t, "").trim().to_string();
        }
        if t == before {
            break;
        }
    }
    if t.is_empty() {
        text.trim().to_string()
    } else {
        t
    }
}

/// A crude English stem, enough to match "fixed" with "fix" in branch names.
pub fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    for suf in ["ing", "ed", "es", "s"] {
        if let Some(r) = w.strip_suffix(suf)
            && r.len() >= 3
        {
            return r.to_string();
        }
    }
    w
}

/// Matches a word to a known repo by its full key, `owner/repo` or bare name.
pub fn resolve_repo(cat: &Catalog, word: &str) -> Option<String> {
    let w = word.to_lowercase();
    if w.len() < 3 {
        return None;
    }
    if cat.repos.contains_key(&w) {
        return Some(w);
    }
    let mut hits = cat
        .repos
        .keys()
        .filter(|k| k.ends_with(&format!("/{w}")) || k.strip_prefix("path:") == Some(w.as_str()));
    let first = hits.next()?.clone();
    // An ambiguous name is not a confident match.
    hits.next().is_none().then_some(first)
}

fn day_start(d: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|x| x.timestamp())
        .unwrap_or(0)
}

fn parse_date(s: &str) -> Option<i64> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(day_start)
        .or_else(|| {
            let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
            let n: i64 = n.parse().ok()?;
            let days = match unit {
                "d" => n,
                "w" => n * 7,
                "m" => n * 30,
                "y" => n * 365,
                _ => return None,
            };
            Some(chrono::Utc::now().timestamp() - days * 86_400)
        })
}

static TIME_RES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?i)\b(today|this morning|tonight)\b", "today"),
        (r"(?i)\byesterday\b", "yesterday"),
        (r"(?i)\bthis week\b", "this_week"),
        (r"(?i)\blast week\b", "last_week"),
        (r"(?i)\b(a )?(couple|few) (of )?(days|weeks) ago\b", "few_weeks"),
        (r"(?i)\bthis month\b", "this_month"),
        (r"(?i)\blast month\b", "last_month"),
        (r"(?i)\b(\d+) (day|week|month)s? ago\b", "n_ago"),
        (r"(?i)\b(?:in|during|back in|around) (january|february|march|april|may|june|july|august|september|october|november|december)(?: (\d{4}))?\b", "month"),
        (r"(?i)\brecently\b", "recently"),
    ]
    .into_iter()
    .map(|(p, k)| (Regex::new(p).unwrap(), k))
    .collect()
});

/// Removes a time phrase from the query and returns the range it names,
/// padded a little because memory of dates is fuzzy.
fn time_phrase(text: &str) -> (String, Option<(i64, i64)>) {
    let today = Local::now().date_naive();
    for (re, kind) in TIME_RES.iter() {
        let Some(c) = re.captures(text) else { continue };
        let (from, to) = match *kind {
            "today" => (today, today + Duration::days(1)),
            "yesterday" => (today - Duration::days(2), today),
            "this_week" => (today - Duration::days(8), today + Duration::days(1)),
            "last_week" => (today - Duration::days(16), today - Duration::days(4)),
            "few_weeks" => (today - Duration::days(30), today),
            "this_month" => (
                today.with_day(1).unwrap() - Duration::days(3),
                today + Duration::days(1),
            ),
            "last_month" => {
                let first = today.with_day(1).unwrap();
                let prev = (first - Duration::days(1)).with_day(1).unwrap();
                (prev - Duration::days(5), first + Duration::days(5))
            }
            "n_ago" => {
                let n: i64 = c[1].parse().unwrap_or(1);
                let days = match c[2].to_lowercase().as_str() {
                    "day" => n,
                    "week" => n * 7,
                    _ => n * 30,
                };
                let center = today - Duration::days(days);
                let pad = Duration::days((days / 3).max(2));
                (center - pad, center + pad)
            }
            "month" => {
                let m = MONTHS
                    .iter()
                    .position(|x| x.eq_ignore_ascii_case(&c[1]))
                    .unwrap() as u32
                    + 1;
                let year = c.get(2).and_then(|y| y.as_str().parse().ok()).unwrap_or(
                    if m <= today.month() {
                        today.year()
                    } else {
                        today.year() - 1
                    },
                );
                let first = NaiveDate::from_ymd_opt(year, m, 1).unwrap();
                let next = if m == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(year, m + 1, 1)
                }
                .unwrap();
                (first - Duration::days(5), next + Duration::days(5))
            }
            _ => (today - Duration::days(21), today + Duration::days(1)),
        };
        let rest = re.replace(text, " ").into_owned();
        return (rest, Some((day_start(from), day_start(to))));
    }
    (text.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(q: &str) -> Parsed {
        let cfg = crate::config::Config::default();
        parse(
            q,
            &Catalog::default(),
            &crate::extract::compile(&cfg.ticket_patterns),
            &cfg.ticket_stoplist,
        )
    }

    #[test]
    fn filler_is_stripped_and_intent_kept() {
        let q = p("find the thread where we fixed the ordering issue");
        assert_eq!(q.text, "fixed the ordering issue");
        assert!(q.kinds.contains(&"bugfix"));
        assert!(q.sentence);
        assert_eq!(
            p("session where we discussed creating history tables").text,
            "discussed creating history tables"
        );
    }

    #[test]
    fn time_phrases_become_a_range_and_leave_the_text() {
        let q = p("the renovate config change last week");
        assert!(q.time_boost.is_some());
        assert!(!q.text.contains("last week"), "{}", q.text);
        let (from, to) = q.time_boost.unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!(from < now - 4 * 86_400 && to < now && to > now - 6 * 86_400);
    }

    #[test]
    fn operators_become_filters() {
        let q = p("agent:codex since:2026-01-02 flaky test");
        assert_eq!(q.filters.agent.as_deref(), Some("codex"));
        assert!(q.filters.since.is_some());
        assert_eq!(q.text, "flaky test");
    }
}
