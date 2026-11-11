pub mod claude;
pub mod codex;

use crate::model::ParsedSession;
use std::path::Path;

pub fn parse(source: &str, path: &Path, bytes: &[u8]) -> ParsedSession {
    match source {
        "claude" => claude::parse(path, bytes),
        _ => codex::parse(path, bytes),
    }
}
