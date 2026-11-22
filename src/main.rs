mod api;
mod config;
mod daemon;
mod demo;
mod eval;
mod extract;
mod ingest;
mod lexical;
mod list;
mod llm;
mod model;
mod models;
mod parse;
mod query;
mod queue;
mod search;
mod search_fmt;
mod store;
mod summarize;
mod util;
mod web;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "recherche",
    version,
    about = "Search your past Claude Code and Codex sessions"
)]
struct Cli {
    /// Config file. Default: $XDG_CONFIG_HOME/recherche/config.toml
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import new session data once and update the indexes.
    Index {
        /// Reparse every archived session and rebuild the indexes.
        #[arg(long)]
        rebuild: bool,
    },
    /// List sessions, newest activity first.
    List {
        /// Restrict to a repository known to the archive.
        #[arg(long)]
        repo: Option<String>,
        /// Restrict to an agent (`claude` or `codex`).
        #[arg(long)]
        agent: Option<String>,
        /// Maximum number of sessions to show.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// YYYY-MM-DD, or a duration such as 7d, 2w, 3m or 1y.
        #[arg(long, value_name = "PERIOD", value_parser = parse_since)]
        since: Option<i64>,
        /// Print a JSON array of session metadata.
        #[arg(long)]
        json: bool,
    },
    /// Search from the command line.
    Search {
        query: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// YYYY-MM-DD, or a duration such as 30d, 8w or 6m.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_vectors: bool,
        #[arg(long)]
        no_rerank: bool,
        /// Also run the LLM rewrite when the query looks like a sentence.
        #[arg(long)]
        rewrite: bool,
        /// Skip the catch-up index before searching.
        #[arg(long)]
        no_index: bool,
    },
    /// Run the indexer and the web UI.
    Serve {
        /// Address to listen on. Default: `listen` from the config, 127.0.0.1:7777.
        #[arg(long)]
        listen: Option<String>,
    },
    /// Open the web UI, optionally with a query.
    Open { query: Vec<String> },
    /// Summarize finished sessions that have no summary yet or have grown
    /// by half since their last one, however old they are.
    Summarize {
        /// Regenerate summaries even for sessions that already have one:
        /// the N sessions with the most recent activity, or every session without N.
        /// Bypasses the inactivity window, including sessions still in progress.
        #[arg(long, value_name = "N", num_args = 0..=1)]
        redo: Option<Option<usize>>,
        /// Show how many sessions and the estimated cost, then stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Score ranking configurations against your eval queries.
    Eval {
        /// Default: $XDG_CONFIG_HOME/recherche/eval.toml
        #[arg(long)]
        file: Option<PathBuf>,
        /// Also score the LLM rewrite (uses the API, cached per query).
        #[arg(long)]
        rewrite: bool,
        /// Print the rank of every query.
        #[arg(long, short)]
        verbose: bool,
    },
    /// Write a launchd agent (macOS) or systemd user unit (Linux) for `serve`.
    InstallService,
    /// Show paths, counts and unparsed lines.
    Doctor,
    /// Serve made-up sessions from demo/sessions, for trying UI changes and
    /// taking screenshots. Ignores your own sessions and never calls an API.
    #[command(hide = true)]
    Demo {
        /// Where the made-up sessions and their index go. Each run replaces
        /// what an earlier run wrote there.
        #[arg(default_value = "/tmp/recherche-demo")]
        dir: PathBuf,
        /// Address to listen on; not the default port, so it can run next
        /// to your own server.
        #[arg(long, default_value = "127.0.0.1:7778")]
        listen: String,
    },
}

fn default_log_filter(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Serve { .. } | Cmd::Demo { .. } | Cmd::Index { .. } | Cmd::Summarize { .. } => {
            "recherche=info"
        }
        Cmd::Search { .. }
        | Cmd::List { .. }
        | Cmd::Open { .. }
        | Cmd::Eval { .. }
        | Cmd::InstallService
        | Cmd::Doctor => "recherche=warn",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RECHERCHE_LOG")
                .unwrap_or_else(|_| default_log_filter(&cli.cmd).into()),
        )
        .with_writer(std::io::stderr)
        // Services send stderr to a log file, where colour codes are noise.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cfg = config::Config::load(cli.config.as_deref())?;
    match cli.cmd {
        Cmd::Index { rebuild } => {
            let engine = search::Engine::open(cfg)?;
            let t = std::time::Instant::now();
            let s = engine.refresh(rebuild)?;
            eprintln!(
                "{} files seen, {} changed, {} sessions updated, {} unparsed lines, in {:.1}s",
                s.files,
                s.changed,
                s.sessions,
                s.bad_lines,
                t.elapsed().as_secs_f32()
            );
        }
        Cmd::List {
            repo,
            agent,
            limit,
            since,
            json,
        } => {
            let engine = search::Engine::open(cfg)?;
            // Listing needs current metadata, but no embeddings.
            if let Err(e) = engine.refresh_with(false, false) {
                tracing::debug!("catch-up index skipped: {e:#}");
            }
            let cat = engine.catalog();
            let (sessions, total) = cat.recent(repo.as_deref(), agent.as_deref(), since, limit);
            if json {
                println!("{}", list::to_json(&cat, &sessions)?);
            } else {
                print!(
                    "{}",
                    list::format_timeline(&cat, &sessions, list::use_color())
                );
            }
            eprintln!("{}", list::footer(sessions.len(), total));
        }
        Cmd::Search {
            query,
            repo,
            agent,
            since,
            limit,
            json,
            no_vectors,
            no_rerank,
            rewrite,
            no_index,
        } => {
            let engine = search::Engine::open(cfg)?;
            if !no_index {
                // With the daemon running, it holds the index writer and the index is current.
                if let Err(e) = engine.refresh(false) {
                    tracing::debug!("catch-up index skipped: {e:#}");
                }
            }
            let mut q = query.join(" ");
            for (k, v) in [("repo", repo), ("agent", agent), ("since", since)] {
                if let Some(v) = v {
                    q.push_str(&format!(" {k}:{v}"));
                }
            }
            let pq = engine.parse(&q);
            let opts = search::Options {
                vectors: !no_vectors && engine.cfg.search.vectors,
                rerank: !no_rerank && engine.cfg.search.rerank,
            };
            let mut res = engine.run(&pq, opts, None, "hybrid")?;
            if rewrite && engine.wants_rewrite(&pq, &res) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                match rt.block_on(engine.rewrite(&pq)) {
                    Ok(rw) => res = engine.run(&pq, opts, Some(&rw), "refined")?,
                    Err(e) => eprintln!("rewrite skipped: {e:#}"),
                }
            }
            print_results(&engine, &res, limit, json)?;
        }
        Cmd::Serve { listen } => {
            let mut cfg = cfg;
            if let Some(l) = listen {
                cfg.listen = l;
            }
            serve(search::Engine::open(cfg)?)?;
        }
        Cmd::Open { query } => {
            let q = query.join(" ");
            let url = if q.is_empty() {
                format!("http://{}/", cfg.listen)
            } else {
                format!("http://{}/?q={}", cfg.listen, web::enc(&q))
            };
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            if std::process::Command::new(opener)
                .arg(&url)
                .status()
                .map(|s| !s.success())
                .unwrap_or(true)
            {
                println!("{url}");
            }
        }
        Cmd::Summarize { redo, dry_run } => {
            let force = redo.is_some();
            let limit = redo.flatten().unwrap_or(usize::MAX);
            let engine = search::Engine::open(cfg)?;
            if let Err(e) = engine.refresh(false) {
                tracing::debug!("catch-up index skipped: {e:#}");
            }
            let c = engine.conn()?;
            // Unlike the daemon, the CLI covers sessions from before `serve` first ran.
            let cands = summarize::candidates(&c, &engine.cfg, true, force)?;
            let n = cands.len().min(limit);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            if n > 0 {
                match rt.block_on(engine.llm.prices()) {
                    Ok(p) => {
                        let est: f64 = cands
                            .iter()
                            .take(limit)
                            .map(|x| llm::summary_estimate(&p, x.digest.len()))
                            .sum();
                        println!("{n} sessions to summarize, about ${est:.2}.");
                    }
                    Err(e) => println!("{n} sessions to summarize (no estimate: {e:#})."),
                }
            } else {
                println!("0 sessions to summarize.");
            }
            if dry_run || n == 0 {
                return Ok(());
            }
            if !engine.llm.summaries_enabled() {
                bail!(
                    "summaries are off: set {} and keep llm.summaries = true",
                    engine.cfg.llm.api_key_env
                );
            }
            let done = rt.block_on(summarize::run(
                &engine.llm,
                &engine.cfg,
                &engine.db,
                true,
                force,
                limit,
                true,
            ))?;
            engine.refresh(false)?;
            println!("Summarized {done} sessions.");
        }
        Cmd::Eval {
            file,
            rewrite,
            verbose,
        } => {
            let engine = search::Engine::open(cfg)?;
            if let Err(e) = engine.refresh(false) {
                tracing::debug!("catch-up index skipped: {e:#}");
            }
            eval::run(
                &engine,
                &file.unwrap_or_else(|| config::config_dir().join("eval.toml")),
                rewrite,
                verbose,
            )?;
        }
        Cmd::InstallService => install_service()?,
        Cmd::Doctor => doctor(cfg)?,
        Cmd::Demo { dir, listen } => {
            let mut cfg = cfg;
            cfg.listen = listen;
            serve(demo::prepare(cfg, &dir)?)?;
        }
    }
    Ok(())
}

/// Runs the indexer in the background and the web UI until stopped.
fn serve(engine: search::Engine) -> Result<()> {
    if !engine.cfg.is_loopback() && engine.cfg.token.is_none() {
        bail!(
            "listening on {} is reachable from other machines; set `token` in the config first",
            engine.cfg.listen
        );
    }
    let engine = Arc::new(engine);
    {
        let c = engine.conn()?;
        if store::get_meta(&c, "auto_summary_since")?.is_none() {
            store::set_meta(
                &c,
                "auto_summary_since",
                &chrono::Utc::now().timestamp().to_string(),
            )?;
        }
    }
    let listen = engine.cfg.listen.clone();
    let e2 = engine.clone();
    std::thread::spawn(move || daemon::run(e2));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(web::serve(engine, &listen))
}

fn parse_since(value: &str) -> Result<i64, String> {
    query::parse_date(value)
        .ok_or_else(|| "expected YYYY-MM-DD or a duration such as 7d, 2w, 3m or 1y".to_string())
}

fn print_results(
    engine: &search::Engine,
    res: &search::Results,
    limit: usize,
    json: bool,
) -> Result<()> {
    let cat = engine.catalog();
    let displayed = search_fmt::collect_hits(&cat, &res.hits, limit);
    for note in &res.notes {
        eprintln!("note: {note}");
    }
    if json {
        println!("{}", search_fmt::to_json(&cat, &displayed)?);
    } else {
        print!(
            "{}",
            search_fmt::format_results(&cat, &displayed, list::use_color())
        );
    }
    eprintln!("{}", search_fmt::footer(displayed.len(), res));
    Ok(())
}

pub fn resume_command(s: &store::SessionMeta) -> String {
    match &s.cwd {
        Some(c) => format!("cd {} && {}", util::shell_quote(c), agent_resume(s)),
        None => agent_resume(s),
    }
}

/// The resume command without the `cd`, for showing the command and its
/// directory on separate lines.
pub fn agent_resume(s: &store::SessionMeta) -> String {
    resume_argv(s).join(" ")
}

/// Claude subagents resume through their parent session.
pub fn resume_argv(s: &store::SessionMeta) -> [&str; 3] {
    match s.source.as_str() {
        "claude" => [
            "claude",
            "--resume",
            s.parent_id.as_deref().unwrap_or(&s.id),
        ],
        _ => ["codex", "resume", &s.id],
    }
}

fn install_service() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();
    let home = std::env::var("HOME")?;
    let log = config::data_dir().join("serve.log");
    std::fs::create_dir_all(config::data_dir())?;
    if cfg!(target_os = "macos") {
        let path = PathBuf::from(&home).join("Library/LaunchAgents/dev.recherche.serve.plist");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.recherche.serve</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
                log = log.display()
            ),
        )?;
        println!(
            "Wrote {}\nStart it with:\n  launchctl bootstrap gui/$(id -u) {}",
            path.display(),
            path.display()
        );
    } else if cfg!(target_os = "linux") {
        let path = PathBuf::from(&home).join(".config/systemd/user/recherche.service");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Unit]\nDescription=recherche session search\n\n[Service]\nExecStart={exe} serve\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
            ),
        )?;
        println!(
            "Wrote {}\nStart it with:\n  systemctl --user daemon-reload && systemctl --user enable --now recherche",
            path.display()
        );
    } else {
        println!(
            "No service manager support for this OS. Run `{exe} serve` from your init system."
        );
    }
    println!(
        "The service does not inherit your shell environment, so OPENROUTER_API_KEY is not set there. For LLM features, add it to the service's environment."
    );
    Ok(())
}

fn doctor(cfg: config::Config) -> Result<()> {
    let engine = search::Engine::open(cfg)?;
    let c = engine.conn()?;
    println!(
        "config       {}",
        config::config_dir().join("config.toml").display()
    );
    println!("data         {}", config::data_dir().display());
    println!("claude       {}", engine.cfg.claude_dir().display());
    println!("codex        {}", engine.cfg.codex_dir().display());
    let mut st = c.prepare("SELECT source, COUNT(*), SUM(skip), SUM(bad_lines), SUM(summary IS NOT NULL) FROM sessions GROUP BY source")?;
    for row in st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })? {
        let (src, n, skip, bad, sum) = row?;
        println!("{src:<12} {n} sessions, {skip} skipped, {sum} summarized, {bad} unparsed lines");
    }
    let gone: i64 = c.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let missing = c
        .prepare("SELECT src_path FROM files")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|p| p.ok())
        .filter(|p| !std::path::Path::new(p).exists())
        .count();
    println!("archive      {gone} files, {missing} kept only in the archive");
    let turns: i64 = c.query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))?;
    let dups: i64 = c.query_row("SELECT COUNT(*) FROM turns WHERE dup = 1", [], |r| r.get(0))?;
    println!("turns        {turns} ({dups} duplicates from forks or replays)");
    println!(
        "index        {} documents, {} vectors",
        engine.lex.num_docs(),
        engine.vecs.read().unwrap().len()
    );
    let key = std::env::var(&engine.cfg.llm.api_key_env).is_ok();
    println!(
        "llm          {} ({} {})",
        if engine.llm.available() { "on" } else { "off" },
        engine.cfg.llm.api_key_env,
        if key { "set" } else { "not set" },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_defaults_and_filters() {
        let cli = Cli::try_parse_from(["recherche", "list"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::List {
                repo: None,
                agent: None,
                limit: 10,
                since: None,
                json: false,
            }
        ));

        let cli = Cli::try_parse_from([
            "recherche",
            "list",
            "--repo",
            "billing",
            "--agent",
            "claude",
            "--limit",
            "125",
            "--since",
            "2026-01-02",
            "--json",
        ])
        .unwrap();
        let Cmd::List {
            repo,
            agent,
            limit,
            since,
            json,
        } = cli.cmd
        else {
            panic!("expected list command");
        };
        assert_eq!(repo.as_deref(), Some("billing"));
        assert_eq!(agent.as_deref(), Some("claude"));
        assert_eq!(limit, 125);
        assert_eq!(since, query::parse_date("2026-01-02"));
        assert!(json);
    }

    #[test]
    fn list_rejects_invalid_arguments() {
        for (flag, value) in [
            ("--limit", "-1"),
            ("--limit", "many"),
            ("--since", "yesterday"),
            ("--since", "2026-02-30"),
            ("--since", "9223372036854775807y"),
        ] {
            assert!(
                Cli::try_parse_from(["recherche", "list", flag, value]).is_err(),
                "accepted {flag} {value}"
            );
        }
    }

    #[test]
    fn quiet_cli_commands_default_to_warn_logging() {
        for argv in [
            ["recherche", "search", "billing"].as_slice(),
            ["recherche", "list"].as_slice(),
            ["recherche", "doctor"].as_slice(),
            ["recherche", "eval"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(default_log_filter(&cli.cmd), "recherche=warn");
        }
        for argv in [
            ["recherche", "serve"].as_slice(),
            ["recherche", "index"].as_slice(),
            ["recherche", "summarize"].as_slice(),
            ["recherche", "demo"].as_slice(),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(default_log_filter(&cli.cmd), "recherche=info");
        }
    }
}
