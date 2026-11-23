//! Formatting for `recherche list`: a date-grouped terminal timeline and JSON output.

use crate::store::{Catalog, SessionMeta};
use crate::util;
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use std::io::IsTerminal;
use std::path::Path;

const GUTTER_WIDTH: usize = 12;
const DEFAULT_COLUMNS: usize = 100;

/// Respects `NO_COLOR`, `CLICOLOR_FORCE`, and whether stdout is a terminal.
pub fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    std::io::stdout().is_terminal()
}

pub fn to_json(cat: &Catalog, sessions: &[&SessionMeta]) -> Result<String> {
    let out: Vec<_> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "agent": s.source,
                "title": s.title,
                "repo": cat.repo_name(s),
                "branch": cat.primary_branch(&s.id),
                "cwd": s.cwd,
                "started": s.started,
                "ended": s.ended,
                "turn_count": s.turn_count,
                "summary": s.summary,
                "kind": s.kind,
                "outcome": s.outcome,
                "resume": crate::resume_command(s),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&out)?)
}

pub fn format_timeline(cat: &Catalog, sessions: &[&SessionMeta], color: bool) -> String {
    format_timeline_at(
        cat,
        sessions,
        Local::now().date_naive(),
        color,
        title_width(),
    )
}

pub fn footer(shown: usize, total: usize) -> String {
    if shown < total {
        format!("showing {shown} of {}", util::count(total, "session"))
    } else {
        format!("{} listed", util::count(shown, "session"))
    }
}

fn title_width() -> usize {
    title_width_for(GUTTER_WIDTH)
}

pub(crate) fn title_width_for(gutter: usize) -> usize {
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_COLUMNS);
    cols.saturating_sub(gutter).clamp(40, 120)
}

pub fn format_timeline_at(
    cat: &Catalog,
    sessions: &[&SessionMeta],
    today: NaiveDate,
    color: bool,
    max_title: usize,
) -> String {
    if sessions.is_empty() {
        return String::new();
    }
    let style = Style { color };
    let groups = group_by_day(sessions);
    let mut out = String::new();
    let sep = style.dim(" · ");

    for (group_idx, (day, items)) in groups.iter().enumerate() {
        if group_idx > 0 {
            out.push('\n');
        }
        out.push_str(&format_day_header(*day, today, &style));
        out.push('\n');

        for (item_idx, (s, time)) in items.iter().enumerate() {
            let is_last = item_idx + 1 == items.len();
            let dot = style.agent(&s.source, "●");
            let title = format_title(&s.title, max_title, &style);
            out.push_str(&format!("  {}  {}  {}\n", style.dim(time), dot, title));

            let meta = metadata_parts(cat, s, &style).join(&sep);
            out.push_str(&format!("         {}  {}\n", style.dim("│"), meta));

            let rail = if is_last { "╵" } else { "│" };
            out.push_str(&format!(
                "         {}  {}\n",
                style.dim(rail),
                style.dim(&crate::resume_command(s))
            ));

            if !is_last {
                out.push_str(&format!("         {}\n", style.dim("│")));
            }
        }
    }
    out
}

fn group_by_day<'a>(
    sessions: &[&'a SessionMeta],
) -> Vec<(Option<NaiveDate>, Vec<(&'a SessionMeta, String)>)> {
    let mut groups: Vec<(Option<NaiveDate>, Vec<(&'a SessionMeta, String)>)> = Vec::new();
    for &s in sessions {
        let dt = (s.ended > 0)
            .then(|| Local.timestamp_opt(s.ended, 0).single())
            .flatten();
        let day = dt.map(|d| d.date_naive());
        let time = dt
            .map(|d| d.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".into());
        if let Some(last) = groups.last_mut()
            && last.0 == day
        {
            last.1.push((s, time));
        } else {
            groups.push((day, vec![(s, time)]));
        }
    }
    groups
}

fn format_day_header(day: Option<NaiveDate>, today: NaiveDate, style: &Style) -> String {
    let Some(d) = day else {
        return style.bold("Unknown date");
    };
    if d == today {
        let rest = format!(" · {}", d.format("%a, %b %-d"));
        format!("{}{}", style.bold("Today"), style.dim(&rest))
    } else if Some(d) == today.pred_opt() {
        let rest = format!(" · {}", d.format("%a, %b %-d"));
        format!("{}{}", style.bold("Yesterday"), style.dim(&rest))
    } else if d.year() == today.year() {
        style.bold(&d.format("%a, %b %-d").to_string())
    } else {
        style.bold(&d.format("%a, %b %-d, %Y").to_string())
    }
}

pub(crate) fn format_title(raw: &str, max_chars: usize, style: &Style) -> String {
    let clean = normalize_whitespace(raw);
    if clean.is_empty() {
        style.dim("Untitled session")
    } else {
        style.bold(&clip_display(&clean, max_chars))
    }
}

fn metadata_parts(cat: &Catalog, s: &SessionMeta, style: &Style) -> Vec<String> {
    let mut parts = Vec::new();

    let agent_label = if s.parent_id.is_some() {
        format!("{} subagent", s.source)
    } else {
        s.source.clone()
    };
    parts.push(style.agent(&s.source, &agent_label));

    let repo = cat.repo_name(s);
    if let Some(ref r) = repo {
        parts.push(r.clone());
    }

    if let Some(branch) = cat.primary_branch(&s.id) {
        parts.push(style.magenta(branch));
    }

    let mut prs = cat.entities_of(&s.id, "pr");
    prs.sort_by_key(|e| -e.strength);
    let repo_lower = repo.as_deref().unwrap_or_default().to_lowercase();
    for p in prs.into_iter().filter(|p| p.strength >= 2).take(2) {
        let label = match p.display.split_once('#') {
            Some((r, n)) if !repo_lower.is_empty() && r.to_lowercase() == repo_lower => {
                format!("PR #{n}")
            }
            _ => p.display.clone(),
        };
        parts.push(style.blue(&label));
    }

    for t in cat.entities_of(&s.id, "ticket").into_iter().take(2) {
        parts.push(style.blue(&t.display));
    }

    if let Some(kind) = &s.kind {
        parts.push(style.dim(kind));
    }
    if let Some(outcome) = &s.outcome {
        let styled = match outcome.as_str() {
            "fixed" | "shipped" => style.green(outcome),
            _ => style.dim(outcome),
        };
        parts.push(styled);
    }

    parts.push(style.dim(&util::count(s.turn_count.max(0) as usize, "turn")));

    if s.cwd.as_ref().is_some_and(|c| !Path::new(c).exists()) {
        parts.push(style.red("dir missing"));
    }

    parts
}

fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn clip_display(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let prefix: String = s.chars().take(keep).collect();
    format!("{}…", prefix.trim_end())
}

pub(crate) struct Style {
    pub(crate) color: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub(crate) fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }

    pub(crate) fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }

    pub(crate) fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }

    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }

    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }

    fn blue(&self, s: &str) -> String {
        self.paint("34", s)
    }

    fn magenta(&self, s: &str) -> String {
        self.paint("35", s)
    }

    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }

    pub(crate) fn agent(&self, source: &str, s: &str) -> String {
        match source {
            "claude" => self.yellow(s),
            "codex" => self.cyan(s),
            "antigravity" => self.blue(s),
            "cursor" => self.green(s),
            _ => self.dim(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Entity, SessionMeta};

    fn local_ts(year: i32, month: u32, day: u32, hour: u32, min: u32) -> i64 {
        let d = NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, min, 0)
            .unwrap();
        Local
            .from_local_datetime(&d)
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn sample_catalog(existing_dir: &Path, missing_dir: &Path) -> (Catalog, Vec<SessionMeta>) {
        let mut cat = Catalog::default();
        cat.repos.insert(
            "github.com/northwind/billing".into(),
            "northwind/billing".into(),
        );
        cat.repos.insert(
            "github.com/northwind/widgets".into(),
            "northwind/widgets".into(),
        );

        let s1 = SessionMeta {
            id: "s1".into(),
            source: "claude".into(),
            cwd: Some(existing_dir.to_string_lossy().into_owned()),
            title: "Fix proration\n  rounding on upgrades".into(),
            summary: Some("- Rounded total once.".into()),
            kind: Some("bugfix".into()),
            outcome: Some("fixed".into()),
            started: local_ts(2026, 11, 19, 9, 10),
            ended: local_ts(2026, 11, 19, 9, 42),
            turn_count: 6,
            repo_key: Some("github.com/northwind/billing".into()),
            ..Default::default()
        };
        let s2 = SessionMeta {
            id: "s2".into(),
            source: "codex".into(),
            cwd: Some(missing_dir.to_string_lossy().into_owned()),
            title: "Add CSV export".into(),
            started: local_ts(2026, 11, 19, 8, 0),
            ended: local_ts(2026, 11, 19, 8, 15),
            turn_count: 1,
            repo_key: Some("github.com/northwind/widgets".into()),
            ..Default::default()
        };
        let s3 = SessionMeta {
            id: "s3".into(),
            source: "claude".into(),
            parent_id: Some("s1".into()),
            cwd: Some(existing_dir.to_string_lossy().into_owned()),
            title: String::new(),
            started: local_ts(2026, 11, 18, 17, 0),
            ended: local_ts(2026, 11, 18, 17, 10),
            turn_count: 3,
            repo_key: Some("github.com/northwind/billing".into()),
            ..Default::default()
        };
        let s4 = SessionMeta {
            id: "s4".into(),
            source: "codex".into(),
            title: "Older session from last year".into(),
            started: local_ts(2025, 10, 5, 14, 0),
            ended: local_ts(2025, 10, 5, 14, 5),
            turn_count: 2,
            ..Default::default()
        };

        cat.entities.insert(
            "s1".into(),
            vec![
                Entity {
                    kind: "branch".into(),
                    key: "fix/proration".into(),
                    display: "fix/proration".into(),
                    origin: "recorded".into(),
                    strength: 1,
                },
                Entity {
                    kind: "pr".into(),
                    key: "northwind/billing#42".into(),
                    display: "northwind/billing#42".into(),
                    origin: "parsed".into(),
                    strength: 4,
                },
                Entity {
                    kind: "pr".into(),
                    key: "northwind/billing#99".into(),
                    display: "northwind/billing#99".into(),
                    origin: "parsed".into(),
                    strength: 1,
                },
                Entity {
                    kind: "ticket".into(),
                    key: "BILL-77".into(),
                    display: "BILL-77".into(),
                    origin: "parsed".into(),
                    strength: 1,
                },
            ],
        );

        let sessions = vec![s1, s2, s3, s4];
        for s in &sessions {
            cat.sessions.insert(s.id.clone(), s.clone());
        }
        (cat, sessions)
    }

    #[test]
    fn timeline_groups_by_day_and_renders_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, sessions) = sample_catalog(dir.path(), &missing);
        let refs: Vec<&SessionMeta> = sessions.iter().collect();
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();

        let rendered = format_timeline_at(&cat, &refs, today, false, 80);
        assert!(!rendered.contains('\x1b'));

        let expected = format!(
            "\
Today · Thu, Nov 19
  09:42  ●  Fix proration rounding on upgrades
         │  claude · northwind/billing · fix/proration · PR #42 · BILL-77 · bugfix · fixed · 6 turns
         │  cd {} && claude --resume s1
         │
  08:15  ●  Add CSV export
         │  codex · northwind/widgets · 1 turn · dir missing
         ╵  cd {} && codex resume s2

Yesterday · Wed, Nov 18
  17:10  ●  Untitled session
         │  claude subagent · northwind/billing · 3 turns
         ╵  cd {} && claude --resume s1

Sun, Oct 5, 2025
  14:05  ●  Older session from last year
         │  codex · 2 turns
         ╵  codex resume s4
",
            util::shell_quote(&dir.path().to_string_lossy()),
            util::shell_quote(&missing.to_string_lossy()),
            util::shell_quote(&dir.path().to_string_lossy()),
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn timeline_applies_ansi_colors_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, sessions) = sample_catalog(dir.path(), &missing);
        let refs: Vec<&SessionMeta> = sessions.iter().take(2).collect();
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();

        let rendered = format_timeline_at(&cat, &refs, today, true, 80);
        assert!(rendered.contains("\x1b[33m●\x1b[0m")); // claude dot
        assert!(rendered.contains("\x1b[36m●\x1b[0m")); // codex dot
        assert!(rendered.contains("\x1b[32mfixed\x1b[0m")); // positive outcome
        assert!(rendered.contains("\x1b[31mdir missing\x1b[0m")); // missing cwd
        assert!(rendered.contains("\x1b[35mfix/proration\x1b[0m")); // branch
    }

    #[test]
    fn json_includes_extended_metadata_fields() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, sessions) = sample_catalog(dir.path(), &missing);
        let refs: Vec<&SessionMeta> = sessions.iter().take(2).collect();

        let text = to_json(&cat, &refs).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["id"], "s1");
        assert_eq!(parsed[0]["agent"], "claude");
        assert_eq!(parsed[0]["repo"], "northwind/billing");
        assert_eq!(parsed[0]["branch"], "fix/proration");
        assert_eq!(parsed[0]["turn_count"], 6);
        assert_eq!(parsed[0]["summary"], "- Rounded total once.");
        assert_eq!(parsed[0]["kind"], "bugfix");
        assert_eq!(parsed[0]["outcome"], "fixed");
        assert_eq!(parsed[1]["branch"], serde_json::Value::Null);
        assert_eq!(parsed[1]["summary"], serde_json::Value::Null);
        assert_eq!(parsed[1]["kind"], serde_json::Value::Null);
        assert_eq!(parsed[1]["outcome"], serde_json::Value::Null);
    }

    #[test]
    fn clip_display_respects_unicode_characters() {
        assert_eq!(clip_display("hello", 5), "hello");
        assert_eq!(clip_display("héllo world", 6), "héllo…");
        assert_eq!(clip_display("a   b\n\tc", 10), "a   b\n\tc");
        assert_eq!(normalize_whitespace("a   b\n\tc"), "a b c");
    }

    #[test]
    fn footer_reports_truncation_and_singular_plural() {
        assert_eq!(footer(0, 0), "0 sessions listed");
        assert_eq!(footer(1, 1), "1 session listed");
        assert_eq!(footer(10, 10), "10 sessions listed");
        assert_eq!(footer(10, 48), "showing 10 of 48 sessions");
    }
}
