use chrono::{Local, TimeZone};
use sha2::{Digest, Sha256};

pub fn hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    let d = h.finalize();
    d[..12].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn fmt_date(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn local(ts: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// "Nov 9", with the year when it isn't this year.
pub fn fmt_day(ts: i64) -> String {
    use chrono::Datelike;
    local(ts)
        .map(|d| {
            if d.year() == Local::now().year() {
                d.format("%b %-d").to_string()
            } else {
                d.format("%b %-d, %Y").to_string()
            }
        })
        .unwrap_or_default()
}

pub fn fmt_time(ts: i64) -> String {
    local(ts)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn fmt_duration(secs: i64) -> String {
    let m = (secs.max(0) + 30) / 60;
    match m {
        0 => "under a minute".into(),
        m if m < 60 => format!("{m} min"),
        m if m < 60 * 24 => match m % 60 {
            0 => format!("{} h", m / 60),
            r => format!("{} h {r} min", m / 60),
        },
        m => match m / (60 * 24) {
            1 => "1 day".into(),
            d => format!("{d} days"),
        },
    }
}

pub fn fmt_ago(ts: i64) -> String {
    let secs = chrono::Utc::now().timestamp() - ts;
    match secs {
        s if s < 3600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 14 => format!("{}d ago", s / 86_400),
        s if s < 86_400 * 60 => format!("{}w ago", s / (86_400 * 7)),
        _ => Local
            .timestamp_opt(ts, 0)
            .single()
            .map(|d| d.format("%b %Y").to_string())
            .unwrap_or_default(),
    }
}

/// Quotes a path for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Splits identifiers for the code-aware field: `fooBar_baz.rs` also yields
/// `foo`, `bar`, `baz` and `rs`, and the whole token is kept.
pub fn expand_idents(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for tok in text.split(|c: char| c.is_whitespace() || "()[]{}<>\"'`,;=:|&!?*".contains(c)) {
        let tok = tok.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.len() < 2 || tok.len() > 120 {
            continue;
        }
        out.push_str(tok);
        out.push(' ');
        if tok.chars().any(|c| !c.is_alphanumeric())
            || tok.chars().any(|c| c.is_uppercase()) && tok.chars().any(|c| c.is_lowercase())
        {
            for part in tok.split(|c: char| !c.is_alphanumeric()) {
                for sub in split_camel(part) {
                    if sub.len() >= 2 && sub.len() < tok.len() {
                        out.push_str(&sub);
                        out.push(' ');
                    }
                }
            }
        }
    }
    out
}

fn split_camel(s: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = i > 0
            && c.is_uppercase()
            && (chars[i - 1].is_lowercase()
                || chars.get(i + 1).is_some_and(|n| n.is_lowercase())
                    && chars[i - 1].is_uppercase());
        if boundary && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_split_but_stay_whole() {
        let e = expand_idents("see parseHTTPResponse in src/history_table.rs");
        for t in [
            "parseHTTPResponse",
            "parse",
            "HTTP",
            "Response",
            "src/history_table.rs",
            "history",
            "table",
            "rs",
        ] {
            assert!(e.split(' ').any(|x| x == t), "{t} missing from {e}");
        }
    }
}
