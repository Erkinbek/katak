//! Notices when another program changes or removes an open file.
//!
//! A background thread looks at the size and modification time of every
//! open file every two seconds (a network drive may take long to answer,
//! so this never runs on the UI thread) and repaints when something
//! changed. Katak keeps showing the version it opened until the user
//! reloads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use katak_core::document::Document;
use katak_core::job::Notify;
use katak_core::source::FileStamp;

const INTERVAL: Duration = Duration::from_secs(2);

/// Locks, ignoring poisoning (the data stays usable).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How a file on disk differs from the open version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Written by another program; `len` is its new size.
    Modified { len: u64 },
    /// Moved or deleted.
    Gone,
}

/// What the last look at a path found, and when it finished.
type Seen = HashMap<PathBuf, (Option<FileStamp>, Instant)>;

pub struct Watcher {
    paths: Arc<Mutex<Vec<PathBuf>>>,
    seen: Arc<Mutex<Seen>>,
}

impl Watcher {
    pub fn start(notify: Notify) -> Watcher {
        let paths: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let (p, s) = (Arc::downgrade(&paths), Arc::downgrade(&seen));
        let _ = std::thread::Builder::new()
            .name("katak-watch".into())
            .spawn(move || {
                // Ends when the app (the owner of the Arcs) is gone.
                while let (Some(paths), Some(seen)) = (p.upgrade(), s.upgrade()) {
                    let list = lock(&paths).clone();
                    drop(paths);
                    let mut changed = false;
                    for path in list {
                        let stamp = FileStamp::of(&path).ok();
                        let mut seen = lock(&seen);
                        let before = seen.get(&path).map(|e| e.0);
                        changed |= before.is_some_and(|b| b != stamp);
                        seen.insert(path, (stamp, Instant::now()));
                    }
                    drop(seen);
                    if changed {
                        notify();
                    }
                    std::thread::sleep(INTERVAL);
                }
            });
        Watcher { paths, seen }
    }

    /// The files to watch (the open tabs).
    pub fn watch(&self, list: Vec<PathBuf>) {
        let mut paths = lock(&self.paths);
        if *paths != list {
            *paths = list;
        }
    }

    /// How the file of `doc` changed since it was (re)opened, judged by a
    /// look that finished after that.
    pub fn change(&self, doc: &Document) -> Option<Change> {
        let seen = lock(&self.seen);
        let (stamp, at) = seen.get(doc.path())?;
        if *at <= doc.opened_at() {
            return None;
        }
        match stamp {
            None => Some(Change::Gone),
            Some(s) if *s != doc.source().stamp() => Some(Change::Modified { len: s.len }),
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use katak_core::document::OpenOptions;
    use katak_core::job::no_notify;

    #[test]
    fn notices_appends_and_deletion() {
        let dir = std::env::temp_dir().join(format!("katak-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.csv");
        std::fs::write(&path, "a,b\n1,2\n").unwrap();
        let doc = Document::open(&path, OpenOptions::default(), no_notify()).unwrap();
        let w = Watcher::start(no_notify());
        w.watch(vec![path.clone()]);
        let wait_for = |want: Option<Change>| {
            let t = Instant::now();
            while w.change(&doc) != want {
                assert!(t.elapsed() < Duration::from_secs(10), "never saw {want:?}");
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        // Unchanged: nothing to report.
        std::thread::sleep(INTERVAL + Duration::from_millis(300));
        assert_eq!(w.change(&doc), None);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"3,4\n"))
            .unwrap();
        wait_for(Some(Change::Modified { len: 12 }));
        drop(doc);
        let doc2 = Document::open(&path, OpenOptions::default(), no_notify()).unwrap();
        std::fs::remove_file(&path).unwrap();
        let t = Instant::now();
        while w.change(&doc2) != Some(Change::Gone) {
            assert!(t.elapsed() < Duration::from_secs(10), "deletion not seen");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(doc2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
