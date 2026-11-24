//! Keeps the index current: a filesystem watcher (FSEvents, inotify or
//! kqueue) plus a stat poll, because watchers miss events after sleep and on
//! network home directories. Also executes on-demand per-session tasks queued
//! from the web UI, writes summaries for idle sessions, and unloads idle
//! models. Runs on one thread, so index writes never race.

use crate::queue::{Task, Wake};
use crate::search::Engine;
use notify::{RecursiveMode, Watcher};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

const SUMMARY_EVERY: Duration = Duration::from_secs(300);
/// Loading the embedder costs a few hundred MB, so new turns of running
/// sessions are embedded in batches rather than on every pass.
const EMBED_EVERY: Duration = Duration::from_secs(120);
const DEBOUNCE: Duration = Duration::from_millis(1500);

pub fn run(e: Arc<Engine>) {
    let (tx, rx) = channel::<Wake>();
    e.tasks.set_notifier(tx.clone());
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev
                .paths
                .iter()
                .any(|p| p.extension().is_some_and(|x| x == "jsonl" || x == "zst"))
        {
            let _ = tx.send(Wake::FileChange);
        }
    })
    .map_err(|err| tracing::warn!("file watcher unavailable, polling only: {err}"))
    .ok();
    if let Some(w) = watcher.as_mut() {
        for dir in [
            e.cfg.claude_dir().join("projects"),
            e.cfg.codex_dir().join("sessions"),
            e.cfg.antigravity_dir().join("brain"),
            e.cfg.cursor_dir().join("projects"),
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
    let mut next_poll = Instant::now();
    let mut run_refresh = true;
    loop {
        drain_tasks(&e, &rt);

        if run_refresh || Instant::now() >= next_poll {
            run_refresh = false;
            next_poll = Instant::now() + poll;
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

            drain_tasks(&e, &rt);

            if last_summary.elapsed() >= SUMMARY_EVERY && e.llm.summaries_enabled() {
                last_summary = Instant::now();
                match rt.block_on(crate::summarize::run_until(
                    &e.llm,
                    &e.cfg,
                    &e.db,
                    crate::summarize::RunOptions {
                        backfill: false,
                        force: false,
                        limit: 20,
                        progress: false,
                    },
                    || e.tasks.has_pending(),
                )) {
                    Ok(0) => {}
                    Ok(_) => {
                        if let Err(err) = e.index_dirty(false) {
                            tracing::warn!("index pass failed: {err:#}");
                        }
                    }
                    Err(err) => {
                        // Earlier summaries may have been saved before this failure.
                        if let Err(index_err) = e.index_dirty(false) {
                            tracing::warn!("index pass failed: {index_err:#}");
                        }
                        tracing::warn!("summaries failed: {err:#}");
                    }
                }
                drain_tasks(&e, &rt);
            }
        }

        if e.tasks.has_pending() {
            continue;
        }
        let wait = next_poll.saturating_duration_since(Instant::now());
        match wait_for_wake(&rx, wait) {
            Some(Wake::Task) => {}
            Some(Wake::FileChange) => run_refresh = true,
            None => {}
        }
    }
}

/// Waits up to `timeout` for a wakeup. File changes debounce briefly so agent
/// write bursts settle, while on-demand tasks interrupt immediately.
fn wait_for_wake(rx: &Receiver<Wake>, timeout: Duration) -> Option<Wake> {
    match rx.recv_timeout(timeout) {
        Ok(Wake::Task) => {
            let mut saw_file = false;
            while let Ok(w) = rx.try_recv() {
                if w == Wake::FileChange {
                    saw_file = true;
                }
            }
            Some(if saw_file {
                Wake::FileChange
            } else {
                Wake::Task
            })
        }
        Ok(Wake::FileChange) => {
            let deadline = Instant::now() + DEBOUNCE;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Some(Wake::FileChange);
                }
                match rx.recv_timeout(left) {
                    Ok(Wake::Task) => return Some(Wake::FileChange),
                    Ok(Wake::FileChange) => {}
                    Err(RecvTimeoutError::Timeout) => return Some(Wake::FileChange),
                    Err(RecvTimeoutError::Disconnected) => return Some(Wake::FileChange),
                }
            }
        }
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => {
            std::thread::sleep(timeout);
            None
        }
    }
}

/// Runs every currently queued on-demand task in FIFO order.
pub(crate) fn drain_tasks(e: &Engine, rt: &tokio::runtime::Runtime) -> usize {
    let mut n = 0;
    while let Some(task) = e.tasks.pop() {
        run_task(e, rt, &task);
        n += 1;
    }
    n
}

fn run_task(e: &Engine, rt: &tokio::runtime::Runtime, task: &Task) {
    let res: anyhow::Result<Option<String>> = match task {
        Task::ReindexSession(id) => e.reindex_session(id).map(|()| None),
        Task::SummarizeSession(id) => rt
            .block_on(crate::summarize::summarize_session(
                &e.llm, &e.cfg, &e.db, id,
            ))
            .and_then(|_| e.index_dirty(false))
            .map(|()| None),
        Task::ReindexAll => e.refresh_with(true, false).map(|s| {
            Some(format!(
                "Re-indexed {}",
                crate::util::count(s.sessions, "session")
            ))
        }),
        Task::SummarizeAll => rt
            .block_on(crate::summarize::run_batch(
                &e.llm,
                &e.cfg,
                &e.db,
                |i, total| e.tasks.set_progress(task, format!("{i}/{total}")),
                || {
                    if let Err(err) = e.index_dirty(false) {
                        tracing::warn!("index pass failed: {err:#}");
                    }
                },
                || e.tasks.is_cancelled(task),
            ))
            .and_then(|r| {
                if let Some(err) = r.error {
                    if r.summarized > 0 {
                        anyhow::bail!(
                            "Summarized {}/{}, then failed: {err}",
                            r.summarized,
                            r.total
                        );
                    }
                    anyhow::bail!("{err}");
                }
                Ok(Some(if r.cancelled {
                    format!("Stopped after {}/{}", r.summarized, r.total)
                } else if r.total == 0 {
                    "All sessions up to date".into()
                } else {
                    format!("Summarized {}", crate::util::count(r.summarized, "session"))
                }))
            }),
    };
    match res {
        Ok(detail) => {
            tracing::info!("completed on-demand {}", task.describe());
            e.tasks.finish_ok(task, detail);
        }
        Err(err) => {
            let msg = format!("{err:#}");
            tracing::warn!("on-demand {} failed: {msg}", task.describe());
            e.tasks.finish_err(task, msg);
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
