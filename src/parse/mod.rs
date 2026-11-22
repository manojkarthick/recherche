pub mod antigravity;
pub mod claude;
pub mod codex;

use crate::model::ParsedSession;
use std::path::Path;

/// Every source, as stored in `sessions.source`.
pub const SOURCES: [&str; 3] = ["claude", "codex", "antigravity"];

/// The static name of a source read back from the database.
pub fn source(name: &str) -> &'static str {
    SOURCES.into_iter().find(|s| *s == name).unwrap_or("codex")
}

pub fn parse(source: &str, path: &Path, bytes: &[u8]) -> ParsedSession {
    match source {
        "claude" => claude::parse(path, bytes),
        "antigravity" => antigravity::parse(path, bytes),
        _ => codex::parse(path, bytes),
    }
}
