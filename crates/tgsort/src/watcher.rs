use crate::{
    ai, notifier,
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
use tgsort_core::{execute, is_ignorable, plan, plan_with_choice, Config, Outcome, Plan};

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

/// Executes (or, in dry-run, just logs) one finished plan.
fn finish(cfg: &Config, p: &Plan, dry_run: bool) -> Result<Res> {
    let name = p
        .source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if dry_run {
        info!("[dry-run] {name} -> {} -> {}", p.rule, p.dest.display());
        return Ok(Res::Left);
    }
    match execute(cfg, p)? {
        Outcome::Moved(d) => {
            info!("moved {name} -> {} [{}]", d.display(), p.rule);
            Ok(Res::Moved(cfg.notify.then_some((name, p.rule.clone()))))
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

/// Processes a batch of ready files; results are index-aligned with `files`.
/// Plain files are moved first; files whose rule uses `{ai}` share one classification call
/// per distinct choices list (none at all while the helper is unavailable).
fn process_many(
    cfg: &Config,
    files: &[PathBuf],
    dry_run: bool,
    gate: &mut ai::Gate,
) -> Vec<Result<Res>> {
    let mut results: Vec<Option<Result<Res>>> = files.iter().map(|_| None).collect();
    let mut ai_jobs: Vec<(usize, Plan)> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        results[i] = match plan(cfg, f) {
            Err(e) => Some(Err(e)),
            Ok(None) => {
                debug!("{}: no matching rule, leaving in place", f.display());
                Some(Ok(Res::Left))
            }
            Ok(Some(p)) if cfg.ai_enabled && !p.ai_choices.is_empty() => {
                ai_jobs.push((i, p));
                None
            }
            Ok(Some(p)) => Some(finish(cfg, &p, dry_run)),
        };
    }
    if !ai_jobs.is_empty() {
        let plans: Vec<&Plan> = ai_jobs.iter().map(|(_, p)| p).collect();
        let choices = ai::resolve(cfg, gate, &plans);
        for ((i, fallback), choice) in ai_jobs.iter().zip(choices) {
            results[*i] = Some(match choice {
                None => finish(cfg, fallback, dry_run),
                Some(c) => plan_with_choice(cfg, &files[*i], Some(&c)).and_then(|p| {
                    let mut p = p.unwrap_or_else(|| fallback.clone());
                    p.rule = format!("{}/{c}", p.rule);
                    finish(cfg, &p, dry_run)
                }),
            });
        }
    }
    results.into_iter().map(|r| r.expect("filled")).collect()
}

/// One-shot pass over files whose mtime is at least `settle_secs` old.
pub fn once(cfg: &Config, dry_run: bool) {
    let settle = Duration::from_secs(cfg.settle_secs);
    let mut ready = Vec::new();
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
            ready.push(f);
        }
    }
    let mut moved = Vec::new();
    let mut gate = ai::Gate::default();
    for (f, r) in ready
        .iter()
        .zip(process_many(cfg, &ready, dry_run, &mut gate))
    {
        match r {
            Ok(Res::Moved(m)) => moved.extend(m),
            Ok(Res::Left) => {}
            Err(e) => warn!("{}: {e:#}", f.display()),
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
    gate: ai::Gate,
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
        let mut files = Vec::new();
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
            } else {
                files.push(f);
            }
        }
        let results = process_many(self.cfg, &files, self.dry_run, &mut self.gate);
        for (f, r) in files.iter().zip(results) {
            match r {
                Ok(Res::Moved(m)) => {
                    moved.extend(m);
                    self.pending.remove(f);
                }
                Ok(Res::Left) => self.pending.handled(f),
                Err(e) => {
                    warn!(
                        "{}: {e:#} (will retry if it changes or in 5 min)",
                        f.display()
                    );
                    self.pending.failed(f, now);
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
        gate: ai::Gate::default(),
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
    if cfg.ai_enabled && cfg.uses_ai() {
        d.gate.available(cfg); // logs availability once
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_source_dir_is_never_listed_or_planned_by_its_parent() {
        let d = tempfile::tempdir().unwrap();
        let dl = d.path().join("Downloads");
        let tg = dl.join("Telegram Desktop");
        std::fs::create_dir_all(&tg).unwrap();
        std::fs::create_dir_all(dl.join("Docs/pdf")).unwrap(); // a created destination folder
        std::fs::write(dl.join("a.pdf"), "x").unwrap();
        std::fs::write(tg.join("b.pdf"), "x").unwrap();
        std::fs::write(dl.join("Docs/pdf/c.pdf"), "x").unwrap();
        assert_eq!(list_files(&dl), vec![dl.join("a.pdf")]);
        assert_eq!(list_files(&tg), vec![tg.join("b.pdf")]);

        let cfg = Config::parse(&format!(
            "[[source]]\ndir='{}'\nroot='{}'\n[[source]]\ndir='{}'\nroot='{}'\n\
             [[rule]]\nname='D'\nextensions=['pdf']\ndest='{{root}}/Docs/{{ext}}'",
            tg.display(),
            d.path().join("T").display(),
            dl.display(),
            dl.display()
        ))
        .unwrap();
        // The directory itself is ignorable, so it can never be moved.
        assert!(plan(&cfg, &tg).unwrap().is_none());
        let p = plan(&cfg, &dl.join("a.pdf")).unwrap().unwrap();
        assert_eq!(p.dest, dl.join("Docs/pdf/a.pdf"));
        let p = plan(&cfg, &tg.join("b.pdf")).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("T/Docs/pdf/b.pdf"));
    }
}
