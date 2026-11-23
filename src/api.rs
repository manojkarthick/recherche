//! JSON access to the existing archive and retrieval engine.

use crate::search::Engine;
use crate::store::{Catalog, SessionMeta};
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn router() -> Router<Arc<Engine>> {
    Router::new()
        .route("/search", get(search))
        .route("/sessions/{id}", get(session))
        .route("/sessions/{id}/turns", get(turns))
        .route("/entity/sessions", get(entity_sessions))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self.0 {
            StatusCode::BAD_REQUEST => "invalid_parameter",
            StatusCode::NOT_FOUND => "not_found",
            _ => "internal_error",
        };
        (
            self.0,
            Json(json!({"error": {"code": code, "message": self.1}})),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        tracing::error!("API request failed: {:#}", error.into());
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Request could not be completed.".into(),
        )
    }
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(flatten)]
    query: crate::web::Params,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct PageParams {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize)]
struct EntityParams {
    kind: String,
    key: String,
    limit: Option<usize>,
    offset: Option<usize>,
}

struct Page {
    limit: usize,
    offset: usize,
}

impl Page {
    fn new(limit: Option<usize>, offset: Option<usize>) -> Result<Self, ApiError> {
        let limit = limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "limit must be between 1 and 100".into(),
            ));
        }
        Ok(Self {
            limit,
            offset: offset.unwrap_or(0),
        })
    }

    fn json(&self, available: usize) -> Value {
        let next = self.offset.saturating_add(self.limit);
        json!({
            "limit": self.limit,
            "offset": self.offset,
            "next_offset": (next < available).then_some(next),
        })
    }
}

// Session details and entity results share the full representation. The catalog
// holds most fields; read the remaining stored metadata without enriching it.
fn session_json(
    engine: &Engine,
    cat: &Catalog,
    session: &SessionMeta,
    conn: &Connection,
) -> anyhow::Result<Value> {
    let (mut original_title, generated_title, title_is_prompt, bytes, bad_lines, summary_turns): (
        Option<String>,
        Option<String>,
        bool,
        i64,
        i64,
        Option<i64>,
    ) = conn.query_row(
        "SELECT title, llm_title, title_is_prompt, bytes, bad_lines, summary_turns
         FROM sessions WHERE id = ?",
        [&session.id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if engine.cfg.ui.hide_system_instructions
        && title_is_prompt
        && let Some(title) = &original_title
        && let Some(t) = crate::store::title_without_system_instructions(conn, &session.id, title)?
    {
        original_title = Some(t);
    }
    let mut children = cat.children.get(&session.id).cloned().unwrap_or_default();
    children.sort();
    let entities: Vec<_> = cat
        .entities
        .get(&session.id)
        .into_iter()
        .flatten()
        .map(|entity| {
            json!({
                "kind": entity.kind, "key": entity.key, "display": entity.display,
                "origin": entity.origin, "strength": entity.strength,
            })
        })
        .collect();
    Ok(json!({
        "id": session.id, "agent": session.source, "parent_id": session.parent_id,
        "children": children, "host": session.host, "cwd": session.cwd,
        "title": session.title, "original_title": original_title,
        "generated_title": generated_title, "title_is_prompt": title_is_prompt,
        "recap": session.recap, "summary": session.summary, "kind": session.kind,
        "outcome": session.outcome, "tags": session.tags, "model": session.model,
        "started": session.started, "ended": session.ended, "turn_count": session.turn_count,
        "bytes": bytes, "bad_lines": bad_lines, "summary_turns": summary_turns,
        "repo_key": session.repo_key, "repo": cat.repo_name(session),
        "repo_origin": session.repo_origin, "entities": entities,
        "resume_argv": crate::resume_argv(session),
    }))
}

async fn search(
    State(engine): State<Arc<Engine>>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) =
        params.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let page = Page::new(params.limit, params.offset)?;
    let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let query = engine.parse(&params.query.full_query());
        let results = engine.run(&query, engine.default_options(), None, "hybrid")?;
        let cat = engine.catalog();
        let conn = engine.conn()?;
        let mut turn_index = conn.prepare("SELECT idx FROM turns WHERE id = ?")?;
        let mut items = Vec::new();
        for hit in results.hits.iter().skip(page.offset).take(page.limit) {
            let Some(session) = cat.sessions.get(&hit.session) else {
                continue;
            };
            let mut matching_turn_indexes = Vec::new();
            for id in hit.turns.iter().take(2) {
                if let Some(index) = turn_index
                    .query_row([id], |row| row.get::<_, i64>(0))
                    .optional()?
                {
                    matching_turn_indexes.push(index);
                }
            }
            items.push(json!({
                "session": {
                    "id": session.id, "agent": session.source, "title": session.title,
                    "repo": cat.repo_name(session), "repo_key": session.repo_key,
                    "started": session.started, "ended": session.ended, "parent_id": session.parent_id,
                },
                "score": hit.score, "reasons": hit.reasons, "rerank": hit.rerank,
                "matching_turn_indexes": matching_turn_indexes,
            }));
        }
        Ok(json!({
            "items": items, "total": results.total, "page": page.json(results.hits.len()),
            "stage": results.stage, "took_ms": results.took_ms, "notes": results.notes,
            "confidence": results.confidence,
        }))
    })
    .await??;
    Ok(Json(body))
}

async fn session(
    State(engine): State<Arc<Engine>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path(id) = id.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let body = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let cat = engine.catalog();
        let session = cat
            .sessions
            .get(&id)
            .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "No session with that ID.".into()))?;
        Ok(session_json(&engine, &cat, session, &engine.conn()?)?)
    })
    .await??;
    Ok(Json(body))
}

async fn turns(
    State(engine): State<Arc<Engine>>,
    id: Result<Path<String>, PathRejection>,
    params: Result<Query<PageParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path(id) = id.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let Query(params) =
        params.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let page = Page::new(params.limit, params.offset)?;
    let body = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let cat = engine.catalog();
        let session = cat.sessions.get(&id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "No session with that ID.".into()))?;
        let conn = engine.conn()?;
        let mut statement = conn.prepare(
            "SELECT idx, ts, branch, cwd, user_text, assistant_text, tool_text, files, commands, errors, tools_json
             FROM turns WHERE session_id = ? ORDER BY idx LIMIT ? OFFSET ?",
        )?;
        // SQLite uses signed offsets. Larger API offsets are necessarily past
        // every stored turn, so represent them as the largest SQLite offset.
        let offset = i64::try_from(page.offset).unwrap_or(i64::MAX);
        let hide = engine.cfg.ui.hide_system_instructions;
        let rows = statement.query_map(rusqlite::params![id, page.limit as i64, offset], |row| {
            let user = row.get::<_, String>(4)?;
            let user = if hide { crate::util::strip_system_instructions(&user).into_owned() } else { user };
            Ok((json!({
                "index": row.get::<_, i64>(0)?, "timestamp": row.get::<_, Option<i64>>(1)?,
                "branch": row.get::<_, Option<String>>(2)?, "cwd": row.get::<_, Option<String>>(3)?,
                "user": user, "assistant": row.get::<_, String>(5)?,
                "tool_text": row.get::<_, String>(6)?, "files": row.get::<_, String>(7)?,
                "commands": row.get::<_, String>(8)?, "errors": row.get::<_, String>(9)?,
            }), row.get::<_, String>(10)?))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (mut item, tools) = row?;
            let tools: Vec<Value> = serde_json::from_str(&tools)?;
            item["tools"] = json!(tools);
            items.push(item);
        }
        Ok(json!({
            "items": items, "total": session.turn_count,
            "page": page.json(session.turn_count as usize),
        }))
    }).await??;
    Ok(Json(body))
}

async fn entity_sessions(
    State(engine): State<Arc<Engine>>,
    params: Result<Query<EntityParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) =
        params.map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.body_text()))?;
    let page = Page::new(params.limit, params.offset)?;
    if params.kind.is_empty() || params.key.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "kind and key must be nonempty".into(),
        ));
    }
    let body = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let cat = engine.catalog();
        let ids = cat.entity_sessions(&params.kind, &params.key);
        let conn = engine.conn()?;
        let items = ids
            .iter()
            .skip(page.offset)
            .take(page.limit)
            .filter_map(|id| cat.sessions.get(id))
            .map(|session| session_json(&engine, &cat, session, &conn))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(json!({"items": items, "total": ids.len(), "page": page.json(ids.len())}))
    })
    .await??;
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use rusqlite::params;
    use tower::ServiceExt;

    fn fixture() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = store::open(&dir.path().join("recherche.db")).unwrap();
        for (id, agent, parent, ended, repo) in [
            ("parent", "claude", None, 300, "github.com/example/project"),
            (
                "child/space 雪",
                "claude",
                Some("parent"),
                400,
                "github.com/example/project",
            ),
            ("codex", "codex", None, 200, "github.com/example/other"),
            ("unknown", "other", None, 100, "github.com/example/other"),
        ] {
            conn.execute(
                "INSERT INTO sessions
                 (id, source, parent_id, host, cwd, title, started, ended, turn_count, repo_key, archive_path)
                 VALUES (?, ?, ?, 'workstation', '/workspace/project', ?, 50, ?, 1, ?, '')",
                params![id, agent, parent, id, ended, repo],
            ).unwrap();
            conn.execute(
                "INSERT INTO turns
                 (session_id, idx, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash)
                 VALUES (?, 0, ?, 'Stored reply', '', '', '', '', '[]', ?, ?)",
                params![id, format!("Context for {id}"), id, id],
            ).unwrap();
        }
        conn.execute_batch(
            r#"UPDATE sessions SET title = 'Original prompt', llm_title = 'Generated parent', title_is_prompt = 1,
             recap = 'Source recap', summary = '- Stored summary', kind = 'bugfix', outcome = 'fixed',
             tags = 'ordering, api', model = 'example-model', bytes = 2048, bad_lines = 2,
             summary_turns = 2, turn_count = 3, repo_origin = 'inferred' WHERE id = 'parent';
             UPDATE turns SET ts = 100, branch = 'fix/ordering', cwd = '/workspace/project/subdir',
             user_text = 'Find quasar ordering **bold** <tag>', assistant_text = '# Stored reply',
             tool_text = 'shell: output', files = 'src/queue.rs', commands = 'first line
second line', errors = 'Stored error',
             tools_json = '[{"name":"shell","input":"first line\nsecond line","output":"<raw output>","error":true}]'
             WHERE session_id = 'parent';
             INSERT INTO turns
             (session_id, idx, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash)
             VALUES ('parent', 1, 'Second prompt', '', '', '', '', '', '[]', 'second', 'second'),
                    ('parent', 2, '', 'Final reply', '', '', '', '', '[]', 'third', 'third');
             INSERT INTO sessions (id, source, host, title, turn_count, skip, archive_path)
             VALUES ('hidden', 'claude', '', 'Excluded', 1, 1, ''), ('empty', 'codex', '', '', 0, 0, '');
             INSERT INTO entities (session_id, kind, key, display, origin, strength)
             VALUES ('parent', 'pr', 'example/project#42', 'example/project#42', 'parsed', 4),
                    ('parent', 'ticket', 'PROJ-123', 'PROJ-123', 'parsed', 1),
                    ('parent', 'file', 'src/a b#雪.rs', 'a b#雪.rs', 'recorded', 1),
                    ('parent', 'branch', 'fix/ordering', 'fix/ordering', 'recorded', 1),
                    ('child/space 雪', 'ticket', 'proj-123', 'PROJ-123', 'parsed', 1),
                    ('codex', 'repo', 'github.com/example/project', 'example/project', 'parsed', 1),
                    ('hidden', 'ticket', 'PROJ-123', 'PROJ-123', 'parsed', 1);"#
        ).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.sources.claude.dir = Some(dir.path().join("claude"));
        cfg.sources.codex.dir = Some(dir.path().join("codex"));
        cfg.sources.antigravity.dir = Some(dir.path().join("antigravity"));
        cfg.search.rerank = false;
        cfg.llm.enabled = false;
        let engine = Engine::open_in(cfg, dir.path()).unwrap();
        engine
            .lex
            .update(&mut conn, engine.cfg.search.boilerplate_min_sessions, false)
            .unwrap();
        engine.lex.reload().unwrap();
        (dir, Arc::new(engine))
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn hidden_system_instructions_match_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        cfg.llm.enabled = false;
        cfg.ui.hide_system_instructions = true;
        let engine = Engine::open_in(cfg, dir.path()).unwrap();
        let conn = engine.conn().unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, source, host, title, title_is_prompt, turn_count, archive_path)
             VALUES ('s', 'claude', '', '<system_instruction>', 1, 1, '');
             INSERT INTO turns
             (session_id, idx, user_text, assistant_text, tool_text, files, commands, errors, tools_json, ukey, chash)
             VALUES ('s', 0, '<system_instruction>
be brief
</system_instruction>

fix the test', '', '', '', '', '', '[]', 'a', 'a');",
        )
        .unwrap();
        *engine.catalog.write().unwrap() = Arc::new(Catalog::load(&conn, true).unwrap());
        let app = crate::web::router(Arc::new(engine));
        let (_, session) = get(&app, "/api/sessions/s").await;
        assert_eq!(session["title"], "fix the test");
        assert_eq!(session["original_title"], "fix the test");
        let (_, turns) = get(&app, "/api/sessions/s/turns").await;
        assert_eq!(turns["items"][0]["user"], "fix the test");
    }

    #[tokio::test]
    async fn session_fields_preserve_stored_data_and_resume_relationships() {
        let (_dir, engine) = fixture();
        let app = crate::web::router(engine);
        let (status, mut body) = get(&app, "/api/sessions/parent").await;
        assert_eq!(status, StatusCode::OK);
        body["entities"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|entity| entity["kind"].as_str().unwrap().to_owned());
        assert_eq!(
            body,
            json!({
                "id": "parent", "agent": "claude", "parent_id": null, "children": ["child/space 雪"],
                "host": "workstation", "cwd": "/workspace/project", "title": "Generated parent",
                "original_title": "Original prompt", "generated_title": "Generated parent", "title_is_prompt": true,
                "recap": "Source recap", "summary": "- Stored summary", "kind": "bugfix", "outcome": "fixed",
                "tags": ["ordering", "api"], "model": "example-model", "started": 50, "ended": 300,
                "turn_count": 3, "bytes": 2048, "bad_lines": 2, "summary_turns": 2,
                "repo_key": "github.com/example/project", "repo": "example/project", "repo_origin": "inferred",
                "entities": [
                    {"kind": "branch", "key": "fix/ordering", "display": "fix/ordering", "origin": "recorded", "strength": 1},
                    {"kind": "file", "key": "src/a b#雪.rs", "display": "a b#雪.rs", "origin": "recorded", "strength": 1},
                    {"kind": "pr", "key": "example/project#42", "display": "example/project#42", "origin": "parsed", "strength": 4},
                    {"kind": "ticket", "key": "PROJ-123", "display": "PROJ-123", "origin": "parsed", "strength": 1}
                ],
                "resume_argv": ["claude", "--resume", "parent"]
            })
        );
        let (status, child) = get(
            &app,
            &format!(
                "/api/sessions/{}",
                crate::web::enc("child/space 雪").replace('+', "%20")
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{child}");
        assert_eq!(
            child["resume_argv"],
            json!(["claude", "--resume", "parent"])
        );
        assert_eq!(child["summary"], Value::Null);
        assert_eq!(child["generated_title"], Value::Null);
        let (_, codex) = get(&app, "/api/sessions/codex").await;
        assert_eq!(codex["resume_argv"], json!(["codex", "resume", "codex"]));
    }

    #[tokio::test]
    async fn search_returns_compact_results_and_reuses_browser_filters() {
        let (_dir, engine) = fixture();
        // Vectors are enabled but absent: lexical results need no model download.
        assert!(engine.cfg.search.vectors);
        let app = crate::web::router(engine);
        let (status, result) = get(&app, "/api/search?q=quasar&limit=1&offset=0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["total"], 1);
        assert_eq!(result["items"][0]["matching_turn_indexes"], json!([0]));
        assert_eq!(result["items"][0].as_object().unwrap().len(), 5);
        assert_eq!(
            result["items"][0]["session"],
            json!({
                "id": "parent", "agent": "claude", "title": "Generated parent",
                "repo": "example/project", "repo_key": "github.com/example/project",
                "started": 50, "ended": 300, "parent_id": null,
            })
        );
        assert!(result["items"][0]["score"].as_f64().unwrap() > 0.0);
        assert!(
            result["items"][0]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "keyword")
        );
        assert_eq!(result["items"][0]["rerank"], Value::Null);
        assert_eq!(result["confidence"], Value::Null);
        assert_eq!(result["notes"], json!([]));
        assert_eq!(result["stage"], "hybrid");
        assert!(result["took_ms"].is_u64());
        let (_, browse) = get(&app, "/api/search").await;
        assert_eq!(browse["total"], 4);
        assert_eq!(browse["items"][0]["session"]["id"], "child/space 雪");
        assert_eq!(browse["items"][0]["matching_turn_indexes"], json!([]));
        let (_, filtered) = get(
            &app,
            "/api/search?repo=example%2Fproject&agent=claude&limit=1&offset=1",
        )
        .await;
        assert_eq!(filtered["total"], 2);
        assert_eq!(filtered["items"][0]["session"]["id"], "parent");
        assert_eq!(filtered["page"]["next_offset"], Value::Null);
        let (_, absent) = get(&app, "/api/search?q=nonexistentquasarword").await;
        assert_eq!(absent["items"], json!([]));
        let (_, after) = get(&app, "/api/search?offset=100").await;
        assert_eq!(after["items"], json!([]));
    }

    #[tokio::test]
    async fn transcript_pagination_preserves_raw_text_and_tool_fields() {
        let (_dir, engine) = fixture();
        let app = crate::web::router(engine);
        let (status, body) = get(&app, "/api/sessions/parent/turns?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "items": [{
                    "index": 0, "timestamp": 100, "branch": "fix/ordering", "cwd": "/workspace/project/subdir",
                    "user": "Find quasar ordering **bold** <tag>", "assistant": "# Stored reply",
                    "tool_text": "shell: output", "files": "src/queue.rs", "commands": "first line\nsecond line",
                    "errors": "Stored error", "tools": [{"name": "shell", "input": "first line\nsecond line", "output": "<raw output>", "error": true}]
                }],
                "total": 3, "page": {"limit": 1, "offset": 0, "next_offset": 1}
            })
        );
        let (_, next) = get(&app, "/api/sessions/parent/turns?limit=1&offset=1").await;
        assert_eq!(next["items"][0]["index"], 1);
        assert_eq!(next["items"][0]["timestamp"], Value::Null);
        assert_eq!(next["items"][0]["cwd"], Value::Null);
        assert_eq!(next["items"][0]["assistant"], "");
        assert_eq!(next["items"][0]["tools"], json!([]));
        let (_, end) = get(&app, "/api/sessions/parent/turns?offset=2").await;
        assert_eq!(end["items"][0]["index"], 2);
        assert_eq!(end["page"]["next_offset"], Value::Null);
        let (_, past) = get(&app, "/api/sessions/parent/turns?offset=20").await;
        assert_eq!(past["items"], json!([]));
        assert_eq!(past["total"], 3);
    }

    #[tokio::test]
    async fn entity_lookup_preserves_existing_membership_and_encoded_keys() {
        let (_dir, engine) = fixture();
        let app = crate::web::router(engine);
        let (status, ticket) = get(
            &app,
            "/api/entity/sessions?kind=ticket&key=PROJ-123&limit=1",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ticket["total"], 2);
        assert_eq!(ticket["items"][0]["id"], "child/space 雪");
        assert_eq!(ticket["page"]["next_offset"], 1);
        let (_, next) = get(
            &app,
            "/api/entity/sessions?kind=ticket&key=proj-123&limit=1&offset=1",
        )
        .await;
        let (_, detail) = get(&app, "/api/sessions/parent").await;
        assert_eq!(next["items"][0], detail);
        for (kind, key, count) in [
            ("repo", "github.com/example/project", 2),
            ("pr", "example/project#42", 1),
            ("file", "src/a b#雪.rs", 1),
            ("branch", "fix/ordering", 1),
            ("commit", "missing", 0),
            ("sha", "missing", 0),
            ("worktree", "/workspace/missing", 0),
            ("unknown", "x", 0),
        ] {
            let (status, body) = get(
                &app,
                &format!(
                    "/api/entity/sessions?kind={kind}&key={}",
                    crate::web::enc(key)
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["total"], count, "{kind}: {key}");
            assert_eq!(body["items"].as_array().unwrap().len(), count as usize);
            assert!(
                !body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["id"] == "codex")
            );
        }
        let (_, past) = get(
            &app,
            "/api/entity/sessions?kind=ticket&key=PROJ-123&offset=20",
        )
        .await;
        assert_eq!(past["items"], json!([]));
        assert_eq!(past["total"], 2);
    }

    #[tokio::test]
    async fn search_pagination_stops_at_the_retained_window() {
        let (_dir, engine) = fixture();
        let conn = engine.conn().unwrap();
        for index in 0..105 {
            conn.execute(
                "INSERT INTO sessions (id, source, host, ended, turn_count, archive_path)
                 VALUES (?, 'codex', '', ?, 1, '')",
                params![format!("bulk-{index:03}"), 1000 + index],
            )
            .unwrap();
        }
        *engine.catalog.write().unwrap() = Arc::new(Catalog::load(&conn, false).unwrap());
        let app = crate::web::router(engine);
        for (offset, length, next_offset) in [(0, 60, Some(60)), (60, 40, None), (100, 0, None)] {
            let (status, body) = get(&app, &format!("/api/search?limit=60&offset={offset}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["total"], 109);
            assert_eq!(body["items"].as_array().unwrap().len(), length);
            assert_eq!(body["page"]["next_offset"], json!(next_offset));
        }
    }

    #[tokio::test]
    async fn invalid_parameters_and_hidden_sessions_return_json_errors() {
        let (_dir, engine) = fixture();
        let app = crate::web::router(engine.clone());
        for endpoint in [
            "/api/search",
            "/api/sessions/parent/turns",
            "/api/entity/sessions?kind=ticket&key=PROJ-123",
        ] {
            let separator = if endpoint.contains('?') { '&' } else { '?' };
            for param in ["limit=0", "limit=101", "limit=no", "offset=-1", "offset=no"] {
                let (status, body) = get(&app, &format!("{endpoint}{separator}{param}")).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}: {param}");
                assert_eq!(body["error"]["code"], "invalid_parameter");
            }
        }
        for query in [
            "",
            "?kind=ticket",
            "?key=PROJ-123",
            "?kind=&key=x",
            "?kind=pr&key=",
        ] {
            let (status, body) = get(&app, &format!("/api/entity/sessions{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"]["code"], "invalid_parameter");
        }
        for id in ["missing", "hidden", "empty"] {
            for suffix in ["", "/turns?offset=20"] {
                let (status, body) = get(&app, &format!("/api/sessions/{id}{suffix}")).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(body["error"]["code"], "not_found");
            }
        }
        engine
            .conn()
            .unwrap()
            .execute("DROP TABLE sessions", [])
            .unwrap();
        let (status, body) = get(&app, "/api/sessions/parent").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body,
            json!({"error": {"code": "internal_error", "message": "Request could not be completed."}})
        );
    }

    #[tokio::test]
    async fn api_routes_use_existing_server_authentication() {
        let (_dir, mut engine) = fixture();
        let cfg = &mut Arc::get_mut(&mut engine).unwrap().cfg;
        cfg.listen = "0.0.0.0:7777".into();
        cfg.token = Some("test-token".into());
        let app = crate::web::router(engine);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/search?token=test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(header::SET_COOKIE));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/search")
                    .header(header::COOKIE, "recherche_token=test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
