//! In-process FIFO queue for on-demand per-session and bulk tasks triggered
//! from the web UI and executed by the background daemon thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Reason the daemon thread should wake from its poll sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    FileChange,
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Task {
    SummarizeSession(String),
    ReindexSession(String),
    SummarizeAll,
    ReindexAll,
}

impl Task {
    pub fn summarize(session_id: impl Into<String>) -> Self {
        Self::SummarizeSession(session_id.into())
    }

    pub fn reindex(session_id: impl Into<String>) -> Self {
        Self::ReindexSession(session_id.into())
    }

    pub fn is_bulk(&self) -> bool {
        matches!(self, Self::SummarizeAll | Self::ReindexAll)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::SummarizeSession(id) => format!("summarize {id}"),
            Self::ReindexSession(id) => format!("reindex {id}"),
            Self::SummarizeAll => "summarize all".into(),
            Self::ReindexAll => "reindex all".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running(Option<String>),
    Done(Option<String>),
    Failed(String),
}

impl TaskState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running(_))
    }
}

const MAX_RETAINED: usize = 512;
const RETAIN_FOR: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
struct Entry {
    state: TaskState,
    at: Instant,
    seq: u64,
}

#[derive(Default)]
struct Inner {
    pending: VecDeque<Task>,
    states: HashMap<Task, Entry>,
    cancelled: HashSet<Task>,
    completed_seq: u64,
    notifier: Option<Sender<Wake>>,
}

#[derive(Default)]
pub struct TaskQueue {
    inner: Mutex<Inner>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Connects the daemon's wakeup channel so enqueued tasks run right away.
    pub fn set_notifier(&self, tx: Sender<Wake>) {
        let mut g = self.inner.lock().unwrap();
        let has_pending = !g.pending.is_empty();
        g.notifier = Some(tx);
        if has_pending && let Some(tx) = &g.notifier {
            let _ = tx.send(Wake::Task);
        }
    }

    /// Monotonic counter incremented every time any task finishes.
    pub fn completed_seq(&self) -> u64 {
        self.inner.lock().unwrap().completed_seq
    }

    /// Queues `task` unless an identical task is already queued or running.
    /// Returns the task's current state.
    pub fn enqueue(&self, task: Task) -> TaskState {
        let mut g = self.inner.lock().unwrap();
        if let Some(e) = g.states.get(&task)
            && e.state.is_active()
        {
            return e.state.clone();
        }
        let now = Instant::now();
        if g.states.len() >= MAX_RETAINED {
            g.states
                .retain(|_, e| e.state.is_active() || now.duration_since(e.at) < RETAIN_FOR);
        }
        g.cancelled.remove(&task);
        let seq = g.completed_seq;
        g.states.insert(
            task.clone(),
            Entry {
                state: TaskState::Queued,
                at: now,
                seq,
            },
        );
        g.pending.push_back(task);
        if let Some(tx) = &g.notifier {
            let _ = tx.send(Wake::Task);
        }
        TaskState::Queued
    }

    /// Takes the next queued task (preferring per-session tasks ahead of bulk
    /// tasks) and marks it running.
    pub fn pop(&self) -> Option<Task> {
        let mut g = self.inner.lock().unwrap();
        let idx = g.pending.iter().position(|t| !t.is_bulk()).unwrap_or(0);
        let task = g.pending.remove(idx)?;
        let seq = g.completed_seq;
        g.states.insert(
            task.clone(),
            Entry {
                state: TaskState::Running(None),
                at: Instant::now(),
                seq,
            },
        );
        Some(task)
    }

    pub fn has_pending(&self) -> bool {
        !self.inner.lock().unwrap().pending.is_empty()
    }

    /// Updates progress text for a running task (such as `"3/12"`).
    pub fn set_progress(&self, task: &Task, detail: impl Into<String>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(e) = g.states.get_mut(task)
            && matches!(e.state, TaskState::Running(_))
        {
            e.state = TaskState::Running(Some(detail.into()));
            e.at = Instant::now();
        }
    }

    /// Cancels a queued task immediately, or signals a running task to stop
    /// after its current step.
    pub fn cancel(&self, task: &Task) -> Option<TaskState> {
        let mut g = self.inner.lock().unwrap();
        if let Some(pos) = g.pending.iter().position(|t| t == task) {
            g.pending.remove(pos);
            g.completed_seq += 1;
            let seq = g.completed_seq;
            let state = TaskState::Done(Some("Cancelled".into()));
            g.states.insert(
                task.clone(),
                Entry {
                    state: state.clone(),
                    at: Instant::now(),
                    seq,
                },
            );
            return Some(state);
        }
        if let Some(state) = g
            .states
            .get(task)
            .filter(|e| matches!(e.state, TaskState::Running(_)))
            .map(|e| e.state.clone())
        {
            g.cancelled.insert(task.clone());
            return Some(state);
        }
        g.states.get(task).map(|e| e.state.clone())
    }

    pub fn is_cancelled(&self, task: &Task) -> bool {
        self.inner.lock().unwrap().cancelled.contains(task)
    }

    pub fn finish_ok(&self, task: &Task, detail: Option<String>) {
        let mut g = self.inner.lock().unwrap();
        g.cancelled.remove(task);
        g.completed_seq += 1;
        let seq = g.completed_seq;
        g.states.insert(
            task.clone(),
            Entry {
                state: TaskState::Done(detail),
                at: Instant::now(),
                seq,
            },
        );
    }

    pub fn finish_err(&self, task: &Task, err: String) {
        let mut g = self.inner.lock().unwrap();
        g.cancelled.remove(task);
        g.completed_seq += 1;
        let seq = g.completed_seq;
        g.states.insert(
            task.clone(),
            Entry {
                state: TaskState::Failed(err),
                at: Instant::now(),
                seq,
            },
        );
    }

    pub fn state(&self, task: &Task) -> Option<TaskState> {
        self.inner
            .lock()
            .unwrap()
            .states
            .get(task)
            .map(|e| e.state.clone())
    }

    /// Returns the state for a poll that started at `since`: active states are
    /// always returned, while `Done`/`Failed` are returned only if the task
    /// finished after `since`.
    pub fn poll_state(&self, task: &Task, since: u64) -> Option<TaskState> {
        let g = self.inner.lock().unwrap();
        let e = g.states.get(task)?;
        if e.state.is_active() || e.seq > since {
            Some(e.state.clone())
        } else {
            None
        }
    }

    /// Active tasks always show up on page load; failures stay visible briefly
    /// so a reload right after an error doesn't hide what happened.
    pub fn page_state(&self, task: &Task, error_ttl: Duration) -> Option<TaskState> {
        let g = self.inner.lock().unwrap();
        let e = g.states.get(task)?;
        match &e.state {
            TaskState::Queued | TaskState::Running(_) => Some(e.state.clone()),
            TaskState::Failed(_) if e.at.elapsed() <= error_ttl => Some(e.state.clone()),
            TaskState::Done(_) | TaskState::Failed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn enqueue_deduplicates_active_tasks_and_allows_retry_after_finish() {
        let q = TaskQueue::new();
        let (tx, rx) = channel();
        q.set_notifier(tx);

        let t = Task::summarize("s1");
        assert_eq!(q.enqueue(t.clone()), TaskState::Queued);
        assert_eq!(q.enqueue(t.clone()), TaskState::Queued);
        assert_eq!(rx.try_recv(), Ok(Wake::Task));
        assert!(rx.try_recv().is_err());

        let popped = q.pop().unwrap();
        assert_eq!(popped, t);
        assert_eq!(q.state(&t), Some(TaskState::Running(None)));
        assert_eq!(q.enqueue(t.clone()), TaskState::Running(None));
        assert!(q.pop().is_none());

        q.finish_err(&t, "boom".into());
        assert_eq!(q.state(&t), Some(TaskState::Failed("boom".into())));
        assert_eq!(
            q.page_state(&t, Duration::from_secs(60)),
            Some(TaskState::Failed("boom".into()))
        );
        assert_eq!(q.page_state(&t, Duration::ZERO), None);

        // Retry after failure re-enqueues cleanly.
        assert_eq!(q.enqueue(t.clone()), TaskState::Queued);
        assert_eq!(rx.try_recv(), Ok(Wake::Task));
        assert_eq!(q.pop(), Some(t.clone()));
        q.finish_ok(&t, None);
        assert_eq!(q.state(&t), Some(TaskState::Done(None)));
        assert_eq!(q.page_state(&t, Duration::from_secs(60)), None);
    }

    #[test]
    fn per_session_tasks_run_ahead_of_bulk_tasks_and_cancel_works() {
        let q = TaskQueue::new();
        let bulk = Task::SummarizeAll;
        let single = Task::reindex("s1");
        q.enqueue(bulk.clone());
        q.enqueue(single.clone());
        assert_eq!(q.pop(), Some(single));
        assert_eq!(
            q.cancel(&bulk),
            Some(TaskState::Done(Some("Cancelled".into())))
        );
        assert!(q.pop().is_none());

        let seq = q.completed_seq();
        q.enqueue(bulk.clone());
        assert_eq!(q.pop(), Some(bulk.clone()));
        q.set_progress(&bulk, "2/5");
        assert_eq!(q.state(&bulk), Some(TaskState::Running(Some("2/5".into()))));
        assert!(!q.is_cancelled(&bulk));
        q.cancel(&bulk);
        assert!(q.is_cancelled(&bulk));
        q.finish_ok(&bulk, Some("Stopped after 2/5".into()));
        assert!(!q.is_cancelled(&bulk));
        assert_eq!(
            q.poll_state(&bulk, seq),
            Some(TaskState::Done(Some("Stopped after 2/5".into())))
        );
        assert_eq!(q.poll_state(&bulk, q.completed_seq()), None);
    }
}
