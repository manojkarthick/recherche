//! Formatting for `recherche search`: minimal ranked cards and JSON output.

use crate::list::{Style, format_title, title_width_for};
use crate::search::{Hit, Results};
use crate::store::{Catalog, SessionMeta};
use crate::util;
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use std::path::Path;

pub fn collect_hits<'a>(
    cat: &'a Catalog,
    hits: &'a [Hit],
    limit: usize,
) -> Vec<(&'a Hit, &'a SessionMeta)> {
    hits.iter()
        .filter_map(|h| cat.sessions.get(&h.session).map(|s| (h, s)))
        .take(limit)
        .collect()
}

pub fn to_json(cat: &Catalog, displayed: &[(&Hit, &SessionMeta)]) -> Result<String> {
    let out: Vec<_> = displayed
        .iter()
        .map(|(h, s)| {
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
                "score": h.score,
                "reasons": h.reasons,
                "resume": crate::resume_command(s),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&out)?)
}

pub fn format_results(cat: &Catalog, displayed: &[(&Hit, &SessionMeta)], color: bool) -> String {
    let rank_width = displayed.len().to_string().len().max(2);
    let indent = rank_width + 2;
    format_results_at(
        cat,
        displayed,
        Local::now().date_naive(),
        color,
        title_width_for(indent),
    )
}

pub fn footer(shown: usize, res: &Results) -> String {
    if shown < res.total {
        format!(
            "showing {shown} of {}, {} ms ({})",
            util::count(res.total, "session"),
            res.took_ms,
            res.stage
        )
    } else {
        format!(
            "{} matched, {} ms ({})",
            util::count(shown, "session"),
            res.took_ms,
            res.stage
        )
    }
}

pub fn format_results_at(
    cat: &Catalog,
    displayed: &[(&Hit, &SessionMeta)],
    today: NaiveDate,
    color: bool,
    max_title: usize,
) -> String {
    if displayed.is_empty() {
        return String::new();
    }
    let style = Style { color };
    let rank_width = displayed.len().to_string().len().max(2);
    let indent = " ".repeat(rank_width + 2);
    let sep = style.dim(" · ");
    let mut out = String::new();

    for (idx, (h, s)) in displayed.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let rank = style.dim(&format!("{:>rank_width$}.", idx + 1));
        let title = format_title(&s.title, max_title, &style);
        out.push_str(&format!("{rank} {title}\n"));

        let meta = metadata_parts(cat, h, s, today, &style).join(&sep);
        out.push_str(&format!("{indent}{meta}\n"));

        out.push_str(&format!(
            "{indent}{}\n",
            style.dim(&crate::resume_command(s))
        ));
    }

    out
}

fn metadata_parts(
    cat: &Catalog,
    h: &Hit,
    s: &SessionMeta,
    today: NaiveDate,
    style: &Style,
) -> Vec<String> {
    let mut parts = Vec::new();

    if let Some(date) = format_date(s.ended, today) {
        parts.push(style.dim(&date));
    }

    let agent_label = if s.parent_id.is_some() {
        format!("{} subagent", s.source)
    } else {
        s.source.clone()
    };
    parts.push(style.agent(&s.source, &agent_label));

    if let Some(repo) = cat.repo_name(s) {
        parts.push(repo);
    }

    if let Some(score) = format_score(h.score) {
        parts.push(style.dim(&score));
    }

    if s.cwd.as_ref().is_some_and(|c| !Path::new(c).exists()) {
        parts.push(style.red("dir missing"));
    }

    parts
}

fn format_date(ts: i64, today: NaiveDate) -> Option<String> {
    if ts <= 0 {
        return None;
    }
    let dt = Local.timestamp_opt(ts, 0).single()?;
    if dt.year() == today.year() {
        Some(dt.format("%b %-d").to_string())
    } else {
        Some(dt.format("%b %-d, %Y").to_string())
    }
}

fn format_score(score: f32) -> Option<String> {
    if score <= 0.0 || !score.is_finite() {
        return None;
    }
    if score < 0.1 {
        Some(format!("{score:.3}"))
    } else {
        Some(format!("{score:.2}"))
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

    fn sample_data(existing_dir: &Path, missing_dir: &Path) -> (Catalog, Vec<Hit>) {
        let mut cat = Catalog::default();
        cat.repos.insert(
            "github.com/northwind/billing-api".into(),
            "northwind/billing-api".into(),
        );

        let s1 = SessionMeta {
            id: "s1".into(),
            source: "claude".into(),
            cwd: Some(existing_dir.to_string_lossy().into_owned()),
            title: "Round EU invoice tax\n  on the total".into(),
            summary: Some("- Rounded total once.".into()),
            kind: Some("bugfix".into()),
            outcome: Some("fixed".into()),
            started: local_ts(2026, 8, 19, 10, 0),
            ended: local_ts(2026, 8, 19, 10, 15),
            turn_count: 4,
            repo_key: Some("github.com/northwind/billing-api".into()),
            ..Default::default()
        };
        let s2 = SessionMeta {
            id: "s2".into(),
            source: "codex".into(),
            cwd: Some(missing_dir.to_string_lossy().into_owned()),
            title: "Prorate mid-cycle plan upgrades".into(),
            started: local_ts(2025, 10, 30, 9, 0),
            ended: local_ts(2025, 10, 30, 9, 30),
            turn_count: 2,
            repo_key: Some("github.com/northwind/billing-api".into()),
            ..Default::default()
        };
        let s3 = SessionMeta {
            id: "s3".into(),
            source: "claude".into(),
            parent_id: Some("s1".into()),
            title: String::new(),
            started: 0,
            ended: 0,
            turn_count: 1,
            ..Default::default()
        };

        cat.entities.insert(
            "s1".into(),
            vec![Entity {
                kind: "branch".into(),
                key: "fix/tax-rounding".into(),
                display: "fix/tax-rounding".into(),
                origin: "recorded".into(),
                strength: 1,
            }],
        );

        for s in [s1, s2, s3] {
            cat.sessions.insert(s.id.clone(), s);
        }

        let hits = vec![
            Hit {
                session: "s1".into(),
                score: 0.942,
                reasons: vec!["keyword".into(), "semantic".into()],
                turns: vec![],
                rerank: Some(0.9),
            },
            Hit {
                session: "s2".into(),
                score: 0.0328,
                reasons: vec!["semantic".into()],
                turns: vec![],
                rerank: None,
            },
            Hit {
                session: "s3".into(),
                score: 0.0,
                reasons: vec![],
                turns: vec![],
                rerank: None,
            },
        ];
        (cat, hits)
    }

    #[test]
    fn ranked_cards_render_clean_copyable_resume_lines() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, hits) = sample_data(dir.path(), &missing);
        let displayed = collect_hits(&cat, &hits, 10);
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();

        let rendered = format_results_at(&cat, &displayed, today, false, 80);
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains("[keyword"));

        let expected = format!(
            concat!(
                " 1. Round EU invoice tax on the total\n",
                "    Aug 19 · claude · northwind/billing-api · 0.94\n",
                "    cd {} && claude --resume s1\n",
                "\n",
                " 2. Prorate mid-cycle plan upgrades\n",
                "    Oct 30, 2025 · codex · northwind/billing-api · 0.033 · dir missing\n",
                "    cd {} && codex resume s2\n",
                "\n",
                " 3. Untitled session\n",
                "    claude subagent\n",
                "    claude --resume s1\n",
            ),
            util::shell_quote(&dir.path().to_string_lossy()),
            util::shell_quote(&missing.to_string_lossy()),
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn ranked_cards_apply_ansi_colors_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, hits) = sample_data(dir.path(), &missing);
        let displayed = collect_hits(&cat, &hits, 2);
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();

        let rendered = format_results_at(&cat, &displayed, today, true, 80);
        assert!(rendered.contains("\x1b[33mclaude\x1b[0m")); // claude agent
        assert!(rendered.contains("\x1b[36mcodex\x1b[0m")); // codex agent
        assert!(rendered.contains("\x1b[31mdir missing\x1b[0m")); // missing cwd
        assert!(rendered.contains("\x1b[1mRound EU invoice tax on the total\x1b[0m"));
    }

    #[test]
    fn json_includes_extended_metadata_fields() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone");
        let (cat, hits) = sample_data(dir.path(), &missing);
        let displayed = collect_hits(&cat, &hits, 2);

        let text = to_json(&cat, &displayed).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["id"], "s1");
        assert_eq!(parsed[0]["agent"], "claude");
        assert_eq!(parsed[0]["repo"], "northwind/billing-api");
        assert_eq!(parsed[0]["branch"], "fix/tax-rounding");
        assert_eq!(parsed[0]["turn_count"], 4);
        assert_eq!(parsed[0]["summary"], "- Rounded total once.");
        assert_eq!(parsed[0]["kind"], "bugfix");
        assert_eq!(parsed[0]["outcome"], "fixed");
        assert_eq!(
            parsed[0]["reasons"],
            serde_json::json!(["keyword", "semantic"])
        );
        assert!(parsed[0]["score"].as_f64().unwrap() > 0.9);
        assert_eq!(parsed[1]["branch"], serde_json::Value::Null);
        assert_eq!(parsed[1]["summary"], serde_json::Value::Null);
        assert_eq!(parsed[1]["kind"], serde_json::Value::Null);
        assert_eq!(parsed[1]["outcome"], serde_json::Value::Null);
    }

    #[test]
    fn footer_reports_truncation_and_singular_plural() {
        let mut res = Results {
            total: 0,
            took_ms: 5,
            stage: "hybrid",
            ..Default::default()
        };
        assert_eq!(footer(0, &res), "0 sessions matched, 5 ms (hybrid)");

        res.total = 1;
        assert_eq!(footer(1, &res), "1 session matched, 5 ms (hybrid)");

        res.total = 4;
        assert_eq!(footer(4, &res), "4 sessions matched, 5 ms (hybrid)");

        res.total = 24;
        assert_eq!(footer(10, &res), "showing 10 of 24 sessions, 5 ms (hybrid)");
    }

    #[test]
    fn rank_alignment_expands_for_hundred_results() {
        let mut cat = Catalog::default();
        let mut hits = Vec::new();
        for i in 0..100 {
            let id = format!("s{i}");
            cat.sessions.insert(
                id.clone(),
                SessionMeta {
                    id: id.clone(),
                    source: "claude".into(),
                    title: format!("Session {i}"),
                    ..Default::default()
                },
            );
            hits.push(Hit {
                session: id,
                score: 0.5,
                reasons: vec![],
                turns: vec![],
                rerank: None,
            });
        }
        let displayed = collect_hits(&cat, &hits, 100);
        let today = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let rendered = format_results_at(&cat, &displayed, today, false, 80);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "  1. Session 0");
        assert_eq!(lines[1], "     claude · 0.50");
        assert_eq!(lines[2], "     claude --resume s0");
        let last_start = lines.len() - 3;
        assert_eq!(lines[last_start], "100. Session 99");
        assert_eq!(lines[last_start + 1], "     claude · 0.50");
    }
}
