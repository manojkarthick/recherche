use chrono::{Datelike, Local, TimeZone};
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

pub fn month_start() -> i64 {
    let now = Local::now();
    Local.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0).single().map(|d| d.timestamp()).unwrap_or(0)
}

pub fn fmt_date(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

pub fn fmt_ago(ts: i64) -> String {
    let secs = chrono::Utc::now().timestamp() - ts;
    match secs {
        s if s < 3600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 14 => format!("{}d ago", s / 86_400),
        s if s < 86_400 * 60 => format!("{}w ago", s / (86_400 * 7)),
        _ => Local.timestamp_opt(ts, 0).single().map(|d| d.format("%b %Y").to_string()).unwrap_or_default(),
    }
}

/// Quotes a path for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c)) {
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
        if tok.chars().any(|c| !c.is_alphanumeric()) || tok.chars().any(|c| c.is_uppercase()) && tok.chars().any(|c| c.is_lowercase()) {
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
            && (chars[i - 1].is_lowercase() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()) && chars[i - 1].is_uppercase());
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
        for t in ["parseHTTPResponse", "parse", "HTTP", "Response", "src/history_table.rs", "history", "table", "rs"] {
            assert!(e.split(' ').any(|x| x == t), "{t} missing from {e}");
        }
    }
}
