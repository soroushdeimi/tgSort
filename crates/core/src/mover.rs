use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};

use crate::{Config, Plan};

const MAX_COLLISIONS: u32 = 10_000;
const EXDEV: i32 = 18;

#[derive(Debug)]
pub enum Outcome {
    /// Final path (may differ from `plan.dest` after collision renaming).
    Moved(PathBuf),
    /// Identical file already exists at this path; source left in place.
    DuplicateSkipped(PathBuf),
    /// Identical file existed at this path; source removed.
    DuplicateDeleted(PathBuf),
}

/// Moves `plan.source` to `plan.dest`, never overwriting and never losing data.
pub fn execute(cfg: &Config, plan: &Plan) -> Result<Outcome> {
    let src = &plan.source;
    let len = fs::metadata(src)
        .with_context(|| format!("cannot stat {}", src.display()))?
        .len();
    ensure!(src.is_file(), "{} is not a regular file", src.display());
    let dir = plan
        .dest
        .parent()
        .context("destination has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    for n in 0..=MAX_COLLISIONS {
        let cand = candidate(&plan.dest, n);
        // create_new reserves the name atomically, so nothing can be overwritten.
        match OpenOptions::new().write(true).create_new(true).open(&cand) {
            Ok(_) => {
                move_file(src, &cand, len).with_context(|| {
                    format!("cannot move {} to {}", src.display(), cand.display())
                })?;
                log::info!(
                    "moved {} -> {} ({})",
                    src.display(),
                    cand.display(),
                    plan.rule
                );
                return Ok(Outcome::Moved(cand));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if is_same_path(src, &cand) {
                    return Ok(Outcome::Moved(cand));
                }
                if same_content(src, &cand, len).unwrap_or_else(|e| {
                    log::warn!("cannot compare with {}: {e}", cand.display());
                    false
                }) {
                    if cfg.delete_duplicates {
                        fs::remove_file(src).with_context(|| {
                            format!("cannot delete duplicate {}", src.display())
                        })?;
                        log::info!(
                            "deleted duplicate {} (same as {})",
                            src.display(),
                            cand.display()
                        );
                        return Ok(Outcome::DuplicateDeleted(cand));
                    }
                    log::info!(
                        "skipped duplicate {} (same as {})",
                        src.display(),
                        cand.display()
                    );
                    return Ok(Outcome::DuplicateSkipped(cand));
                }
            }
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", cand.display())),
        }
    }
    bail!("too many name collisions for {}", plan.dest.display())
}

/// `stem.ext` for n = 0, `stem (n).ext` otherwise.
fn candidate(dest: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return dest.to_path_buf();
    }
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy();
    let name = match dest.extension() {
        Some(ext) => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    dest.with_file_name(name)
}

fn is_same_path(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

fn same_content(a: &Path, b: &Path, len: u64) -> io::Result<bool> {
    Ok(fs::metadata(b)?.len() == len && hash(a)? == hash(b)?)
}

fn hash(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf)? {
            0 => return Ok(h.finalize().into()),
            n => h.update(&buf[..n]),
        }
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(EXDEV)
}

/// `dest` is our own empty placeholder; on failure it is removed again.
fn move_file(src: &Path, dest: &Path, len: u64) -> Result<()> {
    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => copy_move(src, dest, len),
        Err(e) => {
            let _ = fs::remove_file(dest);
            Err(e.into())
        }
    }
}

/// Copy to a hidden temp name, fsync, verify, rename into place, then remove the source.
fn copy_move(src: &Path, dest: &Path, len: u64) -> Result<()> {
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dest.with_file_name(format!(".{name}.tgsort-tmp"));
    let copied = (|| -> Result<()> {
        fs::copy(src, &tmp)?;
        let f = OpenOptions::new().write(true).open(&tmp)?;
        if let Ok(m) = fs::metadata(src).and_then(|m| m.modified()) {
            let _ = f.set_modified(m);
        }
        f.sync_all()?;
        ensure!(f.metadata()?.len() == len, "size mismatch after copy");
        fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if let Err(e) = copied {
        let _ = fs::remove_file(&tmp);
        let _ = fs::remove_file(dest);
        return Err(e);
    }
    fs::remove_file(src)
        .with_context(|| format!("copied, but cannot remove source {}", src.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(delete: bool) -> Config {
        Config::parse(&format!("delete_duplicates = {delete}")).unwrap()
    }

    fn mk(path: &Path, content: &str) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        path.into()
    }

    fn plan_for(source: PathBuf, dest: PathBuf) -> Plan {
        Plan {
            rule: "t".into(),
            source,
            dest,
            ai_choices: Vec::new(),
        }
    }

    #[test]
    fn simple_move_creates_dirs() {
        let d = tempfile::tempdir().unwrap();
        let src = mk(&d.path().join("in/a.mp4"), "data");
        let dest = d.path().join("out/x/y/a.mp4");
        match execute(&cfg(false), &plan_for(src.clone(), dest.clone())).unwrap() {
            Outcome::Moved(p) => assert_eq!(p, dest),
            o => panic!("{o:?}"),
        }
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dest).unwrap(), "data");
    }

    #[test]
    fn collision_renames_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/a.txt"), "old");
        mk(&d.path().join("out/a (1).txt"), "older");
        let src = mk(&d.path().join("in/a.txt"), "new");
        let Outcome::Moved(p) = execute(&cfg(false), &plan_for(src, dest.clone())).unwrap() else {
            panic!()
        };
        assert_eq!(p, d.path().join("out/a (2).txt"));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "old");
        assert_eq!(fs::read_to_string(p).unwrap(), "new");
    }

    #[test]
    fn collision_without_extension() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/README"), "old");
        let src = mk(&d.path().join("in/README"), "new");
        let Outcome::Moved(p) = execute(&cfg(false), &plan_for(src, dest)).unwrap() else {
            panic!()
        };
        assert_eq!(p, d.path().join("out/README (1)"));
    }

    #[test]
    fn duplicate_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/a.txt"), "same");
        let src = mk(&d.path().join("in/a.txt"), "same");
        let o = execute(&cfg(false), &plan_for(src.clone(), dest.clone())).unwrap();
        assert!(matches!(o, Outcome::DuplicateSkipped(p) if p == dest));
        assert!(src.exists());
    }

    #[test]
    fn duplicate_is_deleted_when_enabled() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/a.txt"), "same");
        let src = mk(&d.path().join("in/a.txt"), "same");
        let o = execute(&cfg(true), &plan_for(src.clone(), dest.clone())).unwrap();
        assert!(matches!(o, Outcome::DuplicateDeleted(p) if p == dest));
        assert!(!src.exists());
        assert!(dest.exists());
    }

    #[test]
    fn same_size_different_content_is_not_duplicate() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/a.txt"), "aaaa");
        let src = mk(&d.path().join("in/a.txt"), "bbbb");
        let o = execute(&cfg(true), &plan_for(src.clone(), dest)).unwrap();
        assert!(matches!(o, Outcome::Moved(p) if p == d.path().join("out/a (1).txt")));
        assert!(!src.exists());
    }

    #[test]
    fn duplicate_of_a_renamed_candidate_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let dest = mk(&d.path().join("out/a.txt"), "different");
        let dup = mk(&d.path().join("out/a (1).txt"), "same");
        let src = mk(&d.path().join("in/a.txt"), "same");
        let o = execute(&cfg(false), &plan_for(src, dest)).unwrap();
        assert!(matches!(o, Outcome::DuplicateSkipped(p) if p == dup));
    }

    #[test]
    fn source_already_at_destination_is_untouched() {
        let d = tempfile::tempdir().unwrap();
        let src = mk(&d.path().join("a.txt"), "x");
        let o = execute(&cfg(true), &plan_for(src.clone(), src.clone())).unwrap();
        assert!(matches!(o, Outcome::Moved(p) if p == src));
        assert_eq!(fs::read_to_string(src).unwrap(), "x");
    }

    #[test]
    fn missing_source_and_directory_source_error() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("out/a");
        assert!(execute(&cfg(false), &plan_for(d.path().join("nope"), dest.clone())).is_err());
        assert!(execute(&cfg(false), &plan_for(d.path().to_path_buf(), dest)).is_err());
    }

    #[test]
    fn copy_fallback_moves_and_cleans_up() {
        let d = tempfile::tempdir().unwrap();
        let src = mk(&d.path().join("in/a.bin"), "payload");
        let dest = d.path().join("out/a.bin");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        File::create(&dest).unwrap(); // placeholder, as execute() would reserve
        copy_move(&src, &dest, 7).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "payload");
        assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn copy_fallback_failure_leaves_source_and_no_debris() {
        let d = tempfile::tempdir().unwrap();
        let src = mk(&d.path().join("in/a.bin"), "payload");
        let dest = d.path().join("out/a.bin");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        File::create(&dest).unwrap();
        assert!(copy_move(&src, &dest, 999).is_err()); // wrong expected size
        assert!(src.exists());
        assert_eq!(fs::read_dir(dest.parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn candidate_naming() {
        let p = Path::new("/x/a.tar.gz");
        assert_eq!(candidate(p, 0), p);
        assert_eq!(candidate(p, 3), Path::new("/x/a.tar (3).gz"));
    }
}
