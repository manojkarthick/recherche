//! The normalized shape every source parser produces.

use chrono::{DateTime, Utc};

#[derive(Debug, Default, Clone)]
pub struct ParsedSession {
    pub id: String,
    /// "claude", "codex" or "antigravity".
    pub source: &'static str,
    pub parent_id: Option<String>,
    pub cwd: Option<String>,
    pub started: Option<DateTime<Utc>>,
    pub ended: Option<DateTime<Utc>>,
    /// The title the agent generated or the user gave the session.
    pub title: Option<String>,
    /// The agent's own most recent recap of the session, when it wrote one.
    pub recap: Option<String>,
    pub model: Option<String>,
    pub turns: Vec<Turn>,
    /// Facts the agent recorded in structured fields (not parsed from text).
    pub facts: Vec<Fact>,
    /// Sessions that are not the user's work, such as automatic approval reviews.
    pub skip: bool,
    /// How the session was started: every Claude `entrypoint` seen, or the
    /// Codex `session_meta.source` when it is a string. Matched against the
    /// source's exclude list in the config.
    pub origins: Vec<String>,
    /// Complete lines that were not valid JSON.
    pub bad_lines: usize,
}

#[derive(Debug, Default, Clone)]
pub struct Turn {
    pub ts: Option<DateTime<Utc>>,
    pub user: String,
    pub assistant: String,
    pub tools: Vec<ToolCall>,
    pub branch: Option<String>,
    pub cwd: Option<String>,
}

/// Tools whose input is a shell command.
pub const COMMAND_TOOLS: [&str; 5] = ["Bash", "exec", "shell", "exec_command", "run_command"];

#[derive(Debug, Default, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The command, or a short description of the input.
    pub input: String,
    pub files: Vec<String>,
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub kind: &'static str,
    pub key: String,
    pub display: String,
    /// "recorded", "parsed" or "inferred".
    pub origin: &'static str,
    /// For PRs: 4 opened, 3 linked, 2 pushed-for, 1 mentioned. Otherwise 1.
    pub strength: i32,
}

impl Fact {
    pub fn new(
        kind: &'static str,
        key: impl Into<String>,
        display: impl Into<String>,
        origin: &'static str,
        strength: i32,
    ) -> Self {
        Self {
            kind,
            key: key.into(),
            display: display.into(),
            origin,
            strength,
        }
    }
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Keeps the head and tail of long tool output, plus every line that looks
/// like an error, so "that ENOSPC thing" still matches.
pub fn clip_output(text: &str, head: usize, tail: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= head + tail {
        return clip_chars(text, 4000);
    }
    let mut out: Vec<&str> = lines[..head].to_vec();
    for l in &lines[head..lines.len() - tail] {
        let low = l.to_ascii_lowercase();
        if low.contains("error")
            || low.contains("panic")
            || low.contains("fatal")
            || low.contains("exception")
            || low.contains("failed")
        {
            out.push(l);
        }
    }
    out.push("…");
    out.extend_from_slice(&lines[lines.len() - tail..]);
    clip_chars(&out.join("\n"), 6000)
}

pub fn clip_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
