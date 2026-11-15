//! Keeps the index current: a filesystem watcher (FSEvents, inotify or
//! kqueue) plus a stat poll, because watchers miss events after sleep and on
//! network home directories. Also writes summaries for idle sessions and
//! unloads idle models. Runs on one thread, so index writes never race.

use crate::search::Engine;
use notify::{RecursiveMode, Watcher};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const SUMMARY_EVERY: Duration = Duration::from_secs(300);
/// Loading the embedder costs a few hundred MB, so new turns of running
/// sessions are embedded in batches rather than on every pass.
const EMBED_EVERY: Duration = Duration::from_secs(120);

pub fn run(e: Arc<Engine>) {
    let (tx, rx) = channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|x| x == "jsonl"))
        {
            let _ = tx.send(());
        }
    })
    .map_err(|err| tracing::warn!("file watcher unavailable, polling only: {err}"))
    .ok();
    if let Some(w) = watcher.as_mut() {
        for dir in [
            e.cfg.claude_dir().join("projects"),
            e.cfg.codex_dir().join("sessions"),
        ] {
            if dir.exists()
                && let Err(err) = w.watch(&dir, RecursiveMode::Recursive)
            {
                tracing::warn!("cannot watch {}: {err}", dir.display());
            }
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let poll = Duration::from_secs(e.cfg.sources.poll_seconds.max(5));
    tracing::info!(
        "watching for session changes, polling every {}s",
        poll.as_secs()
    );
    let mut last_summary = Instant::now()
        .checked_sub(SUMMARY_EVERY)
        .unwrap_or_else(Instant::now);
    let mut last_embed = Instant::now()
        .checked_sub(EMBED_EVERY)
        .unwrap_or_else(Instant::now);
    loop {
        let embed = last_embed.elapsed() >= EMBED_EVERY;
        if embed {
            last_embed = Instant::now();
        }
        match e.refresh_with(false, embed) {
            Ok(s) if !s.updated.is_empty() => log_updated(&s.updated),
            Ok(_) => {}
            Err(err) => tracing::warn!("index pass failed: {err:#}"),
        }
        e.models.unload_idle();
        if last_summary.elapsed() >= SUMMARY_EVERY && e.llm.summaries_enabled() {
            last_summary = Instant::now();
            match rt.block_on(crate::summarize::run(
                &e.llm, &e.cfg, &e.db, false, false, 20, false,
            )) {
                Ok(0) => {}
                Ok(_) => {
                    if let Err(err) = e.refresh(false) {
                        tracing::warn!("index pass failed: {err:#}");
                    }
                }
                Err(err) => tracing::warn!("summaries failed: {err:#}"),
            }
        }
        match rx.recv_timeout(poll) {
            // Agents write in bursts; let a burst settle before reindexing.
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(1500));
                while rx.try_recv().is_ok() {}
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(poll),
        }
    }
}

/// Names the sessions an index pass touched. The first pass after install can
/// touch thousands, so only the first few are listed.
fn log_updated(updated: &[String]) {
    const SHOWN: usize = 5;
    let n = updated.len();
    let mut msg = format!(
        "indexed {}: {}",
        crate::util::count(n, "session"),
        updated[..n.min(SHOWN)].join("; ")
    );
    if n > SHOWN {
        msg.push_str(&format!("; and {} more", n - SHOWN));
    }
    tracing::info!("{msg}");
}
