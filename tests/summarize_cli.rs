use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_fails_on_provider_error_and_keeps_completed_summaries() {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = Router::new().route("/chat/completions", post(move || {
        let count = count.clone();
        async move {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Json(json!({"choices": [{"message": {"content": "{\"title\":\"Saved title\",\"summary\":\"- Saved summary.\"}"}}]})).into_response()
            } else {
                (StatusCode::BAD_GATEWAY, Json(json!({"error": {"message": "provider unavailable"}}))).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    let codex = dir.path().join("codex");
    let project = claude.join("projects/repo");
    std::fs::create_dir_all(&project).unwrap();
    for (id, date) in [
        ("s1", "2026-01-01T00:00:00Z"),
        ("s2", "2026-01-02T00:00:00Z"),
    ] {
        let line = json!({"type":"user", "sessionId":id, "cwd":"/work/repo", "timestamp":date, "message":{"role":"user","content":"fix the bug"}});
        std::fs::write(project.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
    }
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("[search]\nvectors = false\nrerank = false\n[sources.claude]\ndir = {:?}\n[sources.codex]\ndir = {:?}\n[sources.antigravity]\ndir = {:?}\n[llm]\nbase_url = \"http://{addr}\"\napi_key_env = \"QA_TEST_API_KEY\"\n", claude, codex, dir.path().join("antigravity"))).unwrap();
    let data = dir.path().join("data");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_recherche"))
        .arg("--config")
        .arg(&config)
        .args(["summarize", "--redo", "2"])
        .env("RECHERCHE_DATA_DIR", &data)
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("CODEX_HOME", &codex)
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env("QA_TEST_API_KEY", "test-key")
        .output()
        .unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!result.status.success(), "{stderr}");
    assert!(
        stderr.contains("failed after 1/2 sessions") && stderr.contains("provider unavailable"),
        "{stderr}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    let c = rusqlite::Connection::open(data.join("recherche.db")).unwrap();
    let (count, dirty): (i64, i64) = c.query_row("SELECT count(*), coalesce(sum(dirty), 0) FROM sessions WHERE summary = '- Saved summary.'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((count, dirty), (1, 0));
    server.abort();
}
