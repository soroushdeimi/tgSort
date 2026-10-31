use crate::{
    notifier,
    pending::{Pending, Snapshot},
};
use anyhow::Result;
use log::{debug, info, warn};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};
use tgsort_core::{execute, is_ignorable, plan, Config, Outcome};

const RESCAN: Duration = Duration::from_secs(300);
const MIN_SLEEP: Duration = Duration::from_millis(50);

/// Top-level regular, non-ignorable files of `dir`.
pub fn list_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| !is_ignorable(p))
        .collect();
    v.sort();
    v
}

fn snapshot(path: &Path) -> Option<Snapshot> {
    let m = fs::metadata(path).ok().filter(|m| m.is_file())?;
    Some(Snapshot {
        size: m.len(),
        mtime: m.modified().ok()?,
    })
}

/// `(file, rule)` for a successful move.
type Moved = (String, String);

enum Res {
    Moved(Option<Moved>),
    Left,
}

/// True if any process still has `path` open (via `lsof`). Falls back to false if lsof is unusable.
fn is_open(path: &Path) -> bool {
    static WARNED: AtomicBool = AtomicBool::new(false);
    match Command::new("lsof").arg("-t").arg("--").arg(path).output() {
        Ok(o) => o.status.success() && !o.stdout.is_empty(),
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                warn!("cannot run lsof ({e}); open-file check disabled");
            }
            false
        }
    }
}

fn process(cfg: &Config, file: &Path, dry_run: bool) -> Result<Res> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(p) = plan(cfg, file)? else {
        debug!("{name}: no matching rule, leaving in place");
        return Ok(Res::Left);
    };
    if dry_run {
        info!("[dry-run] {name} -> {} -> {}", p.rule, p.dest.display());
        return Ok(Res::Left);
    }
    match execute(cfg, &p)? {
        Outcome::Moved(d) => {
            info!("moved {name} -> {} [{}]", d.display(), p.rule);
            Ok(Res::Moved(cfg.notify.then_some((name, p.rule))))
        }
        Outcome::DuplicateSkipped(d) => {
            info!(
                "duplicate of {}, left {name} in place [{}]",
                d.display(),
                p.rule
            );
            Ok(Res::Left)
        }
        Outcome::DuplicateDeleted(d) => {
            info!("duplicate of {}, deleted {name} [{}]", d.display(), p.rule);
            Ok(Res::Moved(None))
        }
    }
}

/// One-shot pass over files whose mtime is at least `settle_secs` old.
pub fn once(cfg: &Config, dry_run: bool) {
    let settle = Duration::from_secs(cfg.settle_secs);
    let mut moved = Vec::new();
    for dir in &cfg.watch_dirs {
        if !dir.is_dir() {
            warn!("watch dir {} does not exist", dir.display());
            continue;
        }
        for f in list_files(dir) {
            let settled = snapshot(&f).is_some_and(|s| {
                s.size > 0
                    && SystemTime::now()
                        .duration_since(s.mtime)
                        .unwrap_or_default()
                        >= settle
            });
            if !settled {
                debug!("{}: not settled yet, skipping", f.display());
                continue;
            }
            if is_open(&f) {
                debug!("{}: still open by another process, skipping", f.display());
                continue;
            }
            match process(cfg, &f, dry_run) {
                Ok(Res::Moved(m)) => moved.extend(m),
                Ok(Res::Left) => {}
                Err(e) => warn!("{}: {e:#}", f.display()),
            }
        }
    }
    notifier::notify_all(&moved);
}

struct Daemon<'a> {
    cfg: &'a Config,
    dry_run: bool,
    pending: Pending,
    /// (canonical, configured) per watch dir, to map FSEvents paths back.
    dirs: Vec<(PathBuf, PathBuf)>,
}

impl Daemon<'_> {
    fn observe(&mut self, path: &Path, now: Instant) {
        if is_ignorable(path) {
            return;
        }
        self.pending.observe(path, snapshot(path), now);
    }

    fn on_event(&mut self, path: &Path, now: Instant) {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        let hit = self
            .dirs
            .iter()
            .find(|(c, _)| *c == parent)
            .map(|(_, o)| o.join(name));
        if let Some(p) = hit {
            self.observe(&p, now);
        }
    }

    fn rescan(&mut self, now: Instant) {
        let seen: HashSet<PathBuf> = self
            .cfg
            .watch_dirs
            .iter()
            .flat_map(|d| list_files(d))
            .collect();
        for p in self.pending.all() {
            if !seen.contains(&p) {
                self.pending.observe(&p, None, now);
            }
        }
        for p in seen {
            self.observe(&p, now);
        }
    }

    fn tick(&mut self, now: Instant) {
        for p in self.pending.waiting() {
            self.pending.observe(&p, snapshot(&p), now);
        }
        let mut moved = Vec::new();
        for f in self
            .pending
            .ready(Duration::from_secs(self.cfg.settle_secs), now)
        {
            if is_open(&f) {
                debug!(
                    "{}: still open by another process, rechecking later",
                    f.display()
                );
                self.pending.touch(&f, now);
                continue;
            }
            match process(self.cfg, &f, self.dry_run) {
                Ok(Res::Moved(m)) => {
                    moved.extend(m);
                    self.pending.remove(&f);
                }
                Ok(Res::Left) => self.pending.handled(&f),
                Err(e) => {
                    warn!(
                        "{}: {e:#} (will retry if it changes or in 5 min)",
                        f.display()
                    );
                    self.pending.failed(&f, now);
                }
            }
        }
        notifier::notify_all(&moved);
    }
}

fn try_watch(
    w: &mut RecommendedWatcher,
    dir: &Path,
    watched: &mut HashSet<PathBuf>,
    warned: &mut HashSet<PathBuf>,
) {
    if watched.contains(dir) {
        return;
    }
    match w.watch(dir, RecursiveMode::NonRecursive) {
        Ok(()) => {
            info!("watching {}", dir.display());
            watched.insert(dir.to_path_buf());
        }
        Err(e) => {
            if warned.insert(dir.to_path_buf()) {
                warn!("cannot watch {} ({e}); will keep retrying", dir.display());
            }
        }
    }
}

pub fn run(cfg: &Config, dry_run: bool) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    let mut d = Daemon {
        cfg,
        dry_run,
        pending: Pending::default(),
        dirs: Vec::new(),
    };
    let (mut watched, mut warned) = (HashSet::new(), HashSet::new());
    let refresh = |d: &mut Daemon,
                   w: &mut RecommendedWatcher,
                   watched: &mut HashSet<_>,
                   warned: &mut HashSet<_>| {
        d.dirs = cfg
            .watch_dirs
            .iter()
            .filter_map(|o| o.canonicalize().ok().map(|c| (c, o.clone())))
            .collect();
        for dir in &cfg.watch_dirs {
            try_watch(w, dir, watched, warned);
        }
    };

    info!("tgsort started{}", if dry_run { " (dry-run)" } else { "" });
    refresh(&mut d, &mut watcher, &mut watched, &mut warned);
    d.rescan(Instant::now());
    let settle = Duration::from_secs(cfg.settle_secs);
    let mut last_scan = Instant::now();
    loop {
        // Idle: sleep until the next rescan. Otherwise wake only when an entry may be due.
        let now = Instant::now();
        let mut wake = last_scan + RESCAN;
        if let Some(due) = d.pending.next_due(settle, now) {
            wake = wake.min(due);
        }
        match rx.recv_timeout(wake.saturating_duration_since(now).max(MIN_SLEEP)) {
            Ok(Ok(ev)) => {
                let now = Instant::now();
                for p in &ev.paths {
                    d.on_event(p, now);
                }
            }
            Ok(Err(e)) => warn!("watch error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("watcher channel closed"),
        }
        let now = Instant::now();
        if d.pending
            .next_due(settle, now)
            .is_some_and(|due| due <= now)
        {
            d.tick(now);
        }
        if now.duration_since(last_scan) >= RESCAN {
            last_scan = now;
            refresh(&mut d, &mut watcher, &mut watched, &mut warned);
            d.rescan(now);
        }
    }
}
