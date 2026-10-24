//! Background jobs.
//!
//! Every operation proportional to the file size runs as a [`Job`] on its
//! own OS thread. A job polls [`JobCtl::is_cancelled`] at least every ~1 MB of
//! work, reports progress in bytes and calls the `notify` callback (the app
//! passes "request a repaint") at most ~30 times per second. Dropping the
//! [`Job`] handle cancels the job; it never blocks the caller.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Change notification callback.
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// A notify callback that does nothing.
pub fn no_notify() -> Notify {
    Arc::new(|| {})
}

/// Lifecycle of a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    Cancelled,
    Failed,
}

/// Shared control block between a job and its handle.
pub struct JobCtl {
    cancel: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
    state: AtomicU8,
    error: Mutex<Option<String>>,
    notify: Notify,
    last_notify_ms: AtomicU64,
    started: Instant,
    finished_after_ms: AtomicU64,
}

const NOTIFY_INTERVAL_MS: u64 = 33;

impl JobCtl {
    fn new(notify: Notify) -> Self {
        JobCtl {
            cancel: AtomicBool::new(false),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            state: AtomicU8::new(JobState::Running as u8),
            error: Mutex::new(None),
            notify,
            last_notify_ms: AtomicU64::new(0),
            started: Instant::now(),
            finished_after_ms: AtomicU64::new(0),
        }
    }

    /// True once cancellation was requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub fn set_done(&self, done: u64) {
        self.done.store(done, Ordering::Relaxed);
    }

    /// Throttled change notification.
    pub fn notify(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_notify_ms.load(Ordering::Relaxed);
        if now >= last + NOTIFY_INTERVAL_MS
            && self
                .last_notify_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            (self.notify)();
        }
    }

    /// Unthrottled change notification.
    pub fn notify_now(&self) {
        (self.notify)();
    }
}

/// Handle to a running job.
pub struct Job {
    ctl: Arc<JobCtl>,
    name: &'static str,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("name", &self.name)
            .field("state", &self.state())
            .finish()
    }
}

impl Job {
    /// Runs `work` on a new thread. An `Err` or a panic marks the job failed.
    pub fn spawn<F>(name: &'static str, notify: Notify, work: F) -> Job
    where
        F: FnOnce(&JobCtl) -> Result<(), String> + Send + 'static,
    {
        let ctl = Arc::new(JobCtl::new(notify));
        let c = ctl.clone();
        let thread = std::thread::Builder::new()
            .name(format!("katak-{name}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(&c)));
                let state = match result {
                    Ok(Ok(())) if c.is_cancelled() => JobState::Cancelled,
                    Ok(Ok(())) => JobState::Done,
                    Ok(Err(_)) if c.is_cancelled() => JobState::Cancelled,
                    Ok(Err(e)) => {
                        *c.error.lock() = Some(e);
                        JobState::Failed
                    }
                    Err(panic) => {
                        let msg = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        *c.error.lock() = Some(format!("internal error: {msg}"));
                        JobState::Failed
                    }
                };
                c.finished_after_ms
                    .store(c.started.elapsed().as_millis() as u64, Ordering::Relaxed);
                c.state.store(state as u8, Ordering::Release);
                (c.notify)();
            })
            .expect("failed to start a worker thread");
        Job {
            ctl,
            name,
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Requests cancellation (non-blocking).
    pub fn cancel(&self) {
        self.ctl.cancel.store(true, Ordering::Relaxed);
    }

    pub fn state(&self) -> JobState {
        match self.ctl.state.load(Ordering::Acquire) {
            0 => JobState::Running,
            1 => JobState::Done,
            2 => JobState::Cancelled,
            _ => JobState::Failed,
        }
    }

    pub fn is_running(&self) -> bool {
        self.state() == JobState::Running
    }

    /// `(done, total)` in the job's unit (usually bytes).
    pub fn progress(&self) -> (u64, u64) {
        (
            self.ctl.done.load(Ordering::Relaxed),
            self.ctl.total.load(Ordering::Relaxed),
        )
    }

    /// Progress in `0.0..=1.0`.
    pub fn fraction(&self) -> f32 {
        let (d, t) = self.progress();
        if t == 0 {
            if self.is_running() { 0.0 } else { 1.0 }
        } else {
            (d as f64 / t as f64).min(1.0) as f32
        }
    }

    pub fn error(&self) -> Option<String> {
        self.ctl.error.lock().clone()
    }

    /// Time since start, or the job's total run time once finished.
    pub fn elapsed(&self) -> Duration {
        if self.is_running() {
            self.ctl.started.elapsed()
        } else {
            Duration::from_millis(self.ctl.finished_after_ms.load(Ordering::Relaxed))
        }
    }

    /// Blocks until the job finishes (tools and tests only; never on the UI thread).
    pub fn join(&self) -> JobState {
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
        self.state()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_reports() {
        let job = Job::spawn("t", no_notify(), |c| {
            c.set_total(10);
            c.set_done(10);
            Ok(())
        });
        assert_eq!(job.join(), JobState::Done);
        assert_eq!(job.progress(), (10, 10));
    }

    #[test]
    fn cancellation_and_failure() {
        let job = Job::spawn("t", no_notify(), |c| {
            while !c.is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        });
        job.cancel();
        assert_eq!(job.join(), JobState::Cancelled);
        let job = Job::spawn("t", no_notify(), |_| Err("boom".into()));
        assert_eq!(job.join(), JobState::Failed);
        assert_eq!(job.error().as_deref(), Some("boom"));
        let job = Job::spawn("t", no_notify(), |_| panic!("bad"));
        assert_eq!(job.join(), JobState::Failed);
        assert!(job.error().unwrap().contains("bad"));
    }
}
