//! Antigravity CLI conversations:
//! `<antigravity_dir>/brain/<id>/.system_generated/logs/transcript_full.jsonl`,
//! one line per step. They are archived as `antigravity/brain/<id>.jsonl`.
//!
//! The transcript has no working directory or title. Antigravity keeps those
//! in `conversation_summaries.db`, and ingest copies a conversation's row to
//! `<id>.meta.json` beside its archived transcript (see `write_meta`), so a
//! rebuild still has them after Antigravity drops the conversation.
//!
//! Tool results carry no call id: each `GENERIC` step is the result of the
//! next unanswered call, in order.

use crate::model::{ParsedSession, ToolCall, Turn, clip_chars, clip_output, parse_ts};
use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

pub fn parse(path: &Path, bytes: &[u8]) -> ParsedSession {
    let mut s = ParsedSession {
        source: "antigravity",
        id: path
            .file_stem()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..Default::default()
    };
    if let Some(meta) = std::fs::read(path.with_extension("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        let text = |k: &str| meta[k].as_str().filter(|t| !t.is_empty()).map(String::from);
        s.title = text("title");
        s.parent_id = text("parent_conversation_id");
        s.cwd = meta["workspace_uris"][0].as_str().and_then(uri_path);
    }
    // (turn, tool) of each call still waiting for its result.
    let mut pending: VecDeque<(usize, usize)> = VecDeque::new();

    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            s.bad_lines += 1;
            continue;
        };
        let ts = v["created_at"].as_str().and_then(parse_ts);
        if let Some(t) = ts {
            s.started = Some(s.started.map_or(t, |x| x.min(t)));
            s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
        }
        match v["type"].as_str().unwrap_or("") {
            "USER_INPUT" => {
                let user = user_text(v["content"].as_str().unwrap_or(""));
                if !user.is_empty() {
                    pending.clear();
                    s.turns.push(Turn {
                        ts,
                        user,
                        cwd: s.cwd.clone(),
                        ..Default::default()
                    });
                }
            }
            "PLANNER_RESPONSE" => {
                if s.model.is_none() {
                    s.model = v["model"].as_str().map(String::from);
                }
                if s.turns.is_empty() {
                    s.turns.push(Turn {
                        ts,
                        cwd: s.cwd.clone(),
                        ..Default::default()
                    });
                }
                let ti = s.turns.len() - 1;
                let turn = &mut s.turns[ti];
                let text = v["content"].as_str().unwrap_or("").trim();
                if !text.is_empty() {
                    if !turn.assistant.is_empty() {
                        turn.assistant.push('\n');
                    }
                    turn.assistant.push_str(text);
                }
                for call in v["tool_calls"].as_array().into_iter().flatten() {
                    pending.push_back((ti, turn.tools.len()));
                    turn.tools.push(tool_call(call));
                }
            }
            "GENERIC" => {
                if let Some((ti, ci)) = pending.pop_front() {
                    attach_result(&mut s.turns[ti].tools[ci], &v);
                }
            }
            _ => {}
        }
    }
    s
}

static USER_REQUEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<USER_REQUEST>(.*?)</USER_REQUEST>").unwrap());

/// The prompt without the metadata and settings blocks Antigravity appends.
fn user_text(content: &str) -> String {
    USER_REQUEST
        .captures(content)
        .map_or(content, |c| c.get(1).unwrap().as_str())
        .trim()
        .to_string()
}

fn tool_call(call: &Value) -> ToolCall {
    let name = call["name"].as_str().unwrap_or("tool").to_string();
    let args = &call["args"];
    let mut files = vec![];
    let input = if name == "run_command" {
        args["CommandLine"].as_str().unwrap_or("").to_string()
    } else if let Some(p) = args["Path"].as_str() {
        files.push(p.to_string());
        p.to_string()
    } else {
        clip_chars(&args.to_string(), 600)
    };
    ToolCall {
        name,
        input,
        files,
        ..Default::default()
    }
}

static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Command exited with code (-?\d+)\.").unwrap());

fn attach_result(tc: &mut ToolCall, step: &Value) {
    let content = step["content"].as_str().unwrap_or("");
    // Every result starts with its timing, sometimes twice.
    let text: String = content
        .lines()
        .skip_while(|l| l.starts_with("Created At: ") || l.starts_with("Completed At: "))
        .collect::<Vec<_>>()
        .join("\n");
    let exit = EXIT_CODE
        .captures(&text)
        .and_then(|c| c[1].parse::<i64>().ok());
    tc.is_error = step["status"].as_str() == Some("ERROR")
        || step["error"].is_string()
        || exit.is_some_and(|e| e != 0);
    // File reads repeat file contents that are already on disk; keep less.
    tc.output = if tc.name == "read_file" {
        clip_output(&text, 5, 0)
    } else {
        clip_output(&text, 20, 20)
    };
}

/// The local path of a `file://` URI.
fn uri_path(uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let hex = raw
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (raw[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Each conversation's metadata, as `<id>.meta.json` will hold it, read from
/// `conversation_summaries.db`. Empty when the database is missing.
pub fn read_meta(antigravity_dir: &Path) -> Result<HashMap<String, String>> {
    let db = antigravity_dir.join("conversation_summaries.db");
    if !db.exists() {
        return Ok(HashMap::new());
    }
    let c = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut st = c.prepare(
        "SELECT conversation_id, title, workspace_uris, parent_conversation_id FROM conversation_summaries",
    )?;
    let rows = st.query_map([], |r| {
        let uris: String = r.get(2)?;
        let meta = json!({
            "title": r.get::<_, String>(1)?,
            "workspace_uris": serde_json::from_str::<Value>(&uris).unwrap_or(json!([])),
            "parent_conversation_id": r.get::<_, String>(3)?,
        });
        Ok((r.get::<_, String>(0)?, meta.to_string()))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Writes the conversation's metadata beside its archived transcript.
/// Returns true when it changed, since the session then needs reparsing even
/// if the transcript did not grow, such as when the title is generated after
/// the last step.
pub fn write_meta(meta: &HashMap<String, String>, archived: &Path) -> Result<bool> {
    let id = archived.file_stem().unwrap_or_default().to_string_lossy();
    let Some(m) = meta.get(id.as_ref()) else {
        return Ok(false);
    };
    let path = archived.with_extension("meta.json");
    if std::fs::read_to_string(&path).is_ok_and(|old| old == *m) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, m)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(i: usize, ty: &str, extra: Value) -> String {
        let mut v = json!({
            "step_index": i, "type": ty, "status": "DONE",
            "created_at": format!("2026-01-01T00:00:{i:02}Z"),
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v.to_string()
    }

    fn transcript() -> String {
        [
            step(0, "USER_INPUT", json!({"source": "USER_EXPLICIT", "content":
                "<USER_REQUEST>\nwhy does `make test` fail?\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-01-01T00:00:00Z.\n</ADDITIONAL_METADATA>"})),
            step(1, "SYSTEM_MESSAGE", json!({"content": "<SYSTEM_MESSAGE>hook output</SYSTEM_MESSAGE>"})),
            step(2, "PLANNER_RESPONSE", json!({"model": "gemini-test", "thinking": "hidden", "tool_calls": [
                {"name": "run_command", "args": {"CommandLine": "make test", "toolSummary": "Run tests"}},
                {"name": "read_file", "args": {"Path": "/work/app/Makefile"}},
            ]})),
            step(3, "GENERIC", json!({"content":
                "Created At: 2026-01-01T00:00:03Z\nCompleted At: 2026-01-01T00:00:04Z\nCommand exited with code 2.\n\nCommand output:\nerror: no rule to make target"})),
            step(4, "GENERIC", json!({"content": "Created At: x\nShowing lines 1-2 of 2\n1: test:\n2: \tcargo test"})),
            step(5, "PLANNER_RESPONSE", json!({"model": "gemini-test", "content": "The `test` target is missing."})),
            step(6, "USER_INPUT", json!({"content": "<USER_REQUEST>\ndelete the build dir\n</USER_REQUEST>"})),
            step(7, "PLANNER_RESPONSE", json!({"tool_calls": [{"name": "run_command", "args": {"CommandLine": "rm -rf build"}}]})),
            step(8, "GENERIC", json!({"status": "ERROR", "error": "tool call denied by pre-tool hook",
                "content": "Encountered error in step execution: tool call denied by pre-tool hook"})),
            step(9, "PLANNER_RESPONSE", json!({"content": "A hook blocked the command."})),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn steps_become_turns_and_results_follow_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conv-1.jsonl");
        let s = parse(&path, transcript().as_bytes());
        assert_eq!(s.id, "conv-1");
        assert_eq!(s.model.as_deref(), Some("gemini-test"));
        assert_eq!(s.turns.len(), 2);
        assert_eq!(s.bad_lines, 0);
        let t = &s.turns[0];
        assert_eq!(t.user, "why does `make test` fail?");
        assert_eq!(t.assistant, "The `test` target is missing.");
        assert_eq!(t.tools[0].input, "make test");
        assert!(t.tools[0].is_error);
        assert!(t.tools[0].output.starts_with("Command exited with code 2."));
        assert_eq!(t.tools[1].files, ["/work/app/Makefile"]);
        assert!(!t.tools[1].is_error);
        assert!(t.tools[1].output.starts_with("Showing lines"));
        let t = &s.turns[1];
        assert_eq!(t.user, "delete the build dir");
        assert!(t.tools[0].is_error);
        assert_eq!(s.started, parse_ts("2026-01-01T00:00:00Z"));
        assert_eq!(s.ended, parse_ts("2026-01-01T00:00:09Z"));
        assert!(s.title.is_none() && s.cwd.is_none());
    }

    #[test]
    fn metadata_comes_from_the_summaries_database() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = rusqlite::Connection::open(dir.path().join("conversation_summaries.db"))?;
        c.execute_batch(
            "CREATE TABLE conversation_summaries (conversation_id TEXT, title TEXT, workspace_uris TEXT, parent_conversation_id TEXT);
             INSERT INTO conversation_summaries VALUES
               ('conv-1', 'Fix make test', '[\"file:///work/my%20app\"]', ''),
               ('conv-2', '', '[]', 'conv-1');",
        )?;
        drop(c);
        let meta = read_meta(dir.path())?;
        let archived = dir.path().join("archive/brain/conv-1.jsonl");
        assert!(write_meta(&meta, &archived)?);
        assert!(
            !write_meta(&meta, &archived)?,
            "unchanged metadata rewritten"
        );
        let s = parse(&archived, transcript().as_bytes());
        assert_eq!(s.title.as_deref(), Some("Fix make test"));
        assert_eq!(s.cwd.as_deref(), Some("/work/my app"));
        assert_eq!(s.turns[0].cwd.as_deref(), Some("/work/my app"));
        assert!(s.parent_id.is_none());

        let child = dir.path().join("archive/brain/conv-2.jsonl");
        write_meta(&meta, &child)?;
        let s = parse(&child, b"");
        assert_eq!(s.parent_id.as_deref(), Some("conv-1"));
        assert!(s.title.is_none() && s.cwd.is_none());

        assert!(!write_meta(
            &meta,
            &dir.path().join("archive/brain/gone.jsonl")
        )?);
        assert!(read_meta(&dir.path().join("missing"))?.is_empty());
        Ok(())
    }
}
