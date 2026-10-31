//! Pure settle tracking: a file is ready once size+mtime are stable for `settle`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const RETRY_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub size: u64,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    /// Processed (or deliberately left alone); stays quiet until the file changes.
    Handled,
    Failed(Instant),
}

#[derive(Debug)]
struct Entry {
    snap: Snapshot,
    last_change: Instant,
    state: State,
}

#[derive(Debug, Default)]
pub struct Pending {
    map: HashMap<PathBuf, Entry>,
}

impl Pending {
    /// Record the latest stat of `path` (`None` = file is gone).
    pub fn observe(&mut self, path: &Path, snap: Option<Snapshot>, now: Instant) {
        let Some(snap) = snap else {
            self.map.remove(path);
            return;
        };
        match self.map.get_mut(path) {
            Some(e) if e.snap == snap => {}
            Some(e) => *e = Entry { snap, last_change: now, state: State::Waiting },
            None => {
                self.map.insert(path.to_path_buf(), Entry { snap, last_change: now, state: State::Waiting });
            }
        }
    }

    /// Paths that are settled and due for processing.
    pub fn ready(&mut self, settle: Duration, now: Instant) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (p, e) in &mut self.map {
            if let State::Failed(at) = e.state {
                if now.duration_since(at) >= RETRY_AFTER {
                    e.state = State::Waiting;
                }
            }
            if e.state == State::Waiting && e.snap.size > 0 && now.duration_since(e.last_change) >= settle {
                out.push(p.clone());
            }
        }
        out.sort();
        out
    }

    /// Treat a still-open file as freshly changed so it is rechecked after another settle period.
    pub fn touch(&mut self, path: &Path, now: Instant) {
        if let Some(e) = self.map.get_mut(path) {
            if e.state == State::Waiting {
                e.last_change = now;
            }
        }
    }

    /// Earliest instant (never before `now`) at which some entry may become ready or be retried.
    /// Empty files are excluded: only a change event/rescan can make them ready.
    pub fn next_due(&self, settle: Duration, now: Instant) -> Option<Instant> {
        self.map
            .values()
            .filter_map(|e| match e.state {
                State::Waiting if e.snap.size > 0 => Some(e.last_change + settle),
                State::Failed(at) => Some(at + RETRY_AFTER),
                _ => None,
            })
            .min()
            .map(|t| t.max(now))
    }

    pub fn handled(&mut self, path: &Path) {
        if let Some(e) = self.map.get_mut(path) {
            e.state = State::Handled;
        }
    }

    pub fn failed(&mut self, path: &Path, now: Instant) {
        if let Some(e) = self.map.get_mut(path) {
            e.state = State::Failed(now);
        }
    }

    pub fn remove(&mut self, path: &Path) {
        self.map.remove(path);
    }

    /// Paths still waiting to settle (the ones worth re-stat'ing every tick).
    pub fn waiting(&self) -> Vec<PathBuf> {
        self.map.iter().filter(|(_, e)| e.state == State::Waiting).map(|(p, _)| p.clone()).collect()
    }

    pub fn all(&self) -> Vec<PathBuf> {
        self.map.keys().cloned().collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(3);

    fn snap(size: u64, secs: u64) -> Snapshot {
        Snapshot { size, mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs) }
    }

    #[test]
    fn settles_after_quiet_period() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a.zip");
        p.observe(f, Some(snap(10, 1)), t0);
        assert!(p.ready(S, t0 + Duration::from_secs(2)).is_empty());
        assert_eq!(p.ready(S, t0 + S), vec![f.to_path_buf()]);
    }

    #[test]
    fn change_resets_timer() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a.zip");
        p.observe(f, Some(snap(10, 1)), t0);
        p.observe(f, Some(snap(20, 2)), t0 + Duration::from_secs(2));
        assert!(p.ready(S, t0 + Duration::from_secs(4)).is_empty());
        assert_eq!(p.ready(S, t0 + Duration::from_secs(5)).len(), 1);
    }

    #[test]
    fn same_snapshot_does_not_reset() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a.zip");
        p.observe(f, Some(snap(10, 1)), t0);
        p.observe(f, Some(snap(10, 1)), t0 + Duration::from_secs(2));
        assert_eq!(p.ready(S, t0 + S).len(), 1);
    }

    #[test]
    fn empty_files_never_ready() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        p.observe(Path::new("/d/e"), Some(snap(0, 1)), t0);
        assert!(p.ready(S, t0 + Duration::from_secs(100)).is_empty());
    }

    #[test]
    fn handled_is_quiet_until_changed() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a");
        p.observe(f, Some(snap(5, 1)), t0);
        p.handled(f);
        p.observe(f, Some(snap(5, 1)), t0 + Duration::from_secs(30));
        assert!(p.ready(S, t0 + Duration::from_secs(60)).is_empty());
        p.observe(f, Some(snap(6, 2)), t0 + Duration::from_secs(60));
        assert_eq!(p.ready(S, t0 + Duration::from_secs(64)).len(), 1);
    }

    #[test]
    fn failed_retries_after_five_minutes_or_on_change() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a");
        p.observe(f, Some(snap(5, 1)), t0);
        p.failed(f, t0 + S);
        assert!(p.ready(S, t0 + Duration::from_secs(100)).is_empty());
        assert_eq!(p.ready(S, t0 + S + RETRY_AFTER).len(), 1);
        p.failed(f, t0);
        p.observe(f, Some(snap(7, 3)), t0 + Duration::from_secs(10));
        assert_eq!(p.ready(S, t0 + Duration::from_secs(13)).len(), 1);
    }

    #[test]
    fn touch_defers_readiness() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a");
        p.observe(f, Some(snap(5, 1)), t0);
        assert_eq!(p.ready(S, t0 + S).len(), 1);
        p.touch(f, t0 + S);
        assert!(p.ready(S, t0 + S + Duration::from_secs(2)).is_empty());
        assert_eq!(p.ready(S, t0 + S + S).len(), 1);
    }

    #[test]
    fn next_due_covers_waiting_failed_and_idle() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        assert_eq!(p.next_due(S, t0), None);
        let (a, b, c, d) = (Path::new("/a"), Path::new("/b"), Path::new("/c"), Path::new("/d"));
        p.observe(a, Some(snap(5, 1)), t0 + Duration::from_secs(2));
        p.observe(b, Some(snap(0, 1)), t0);
        assert_eq!(p.next_due(S, t0), Some(t0 + Duration::from_secs(2) + S));
        p.observe(c, Some(snap(5, 1)), t0);
        assert_eq!(p.next_due(S, t0), Some(t0 + S));
        assert_eq!(p.next_due(S, t0 + Duration::from_secs(10)), Some(t0 + Duration::from_secs(10)));
        p.handled(c);
        p.failed(a, t0);
        assert_eq!(p.next_due(S, t0), Some(t0 + RETRY_AFTER));
        p.observe(d, Some(snap(1, 1)), t0);
        p.handled(d);
        p.handled(a);
        assert_eq!(p.next_due(S, t0), None);
    }

    #[test]
    fn vanished_files_are_dropped() {
        let (mut p, t0) = (Pending::default(), Instant::now());
        let f = Path::new("/d/a");
        p.observe(f, Some(snap(5, 1)), t0);
        p.observe(f, None, t0);
        assert_eq!(p.len(), 0);
    }
}
