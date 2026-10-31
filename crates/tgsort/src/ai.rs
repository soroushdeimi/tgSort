//! Optional on-device AI classification through the `tgsort-ai` helper (see AI_PROTOCOL.md).
//!
//! The helper is spawned only when a file needs a choice, once per rule group per batch, and
//! exits by itself. Every failure degrades to "no choice" (the `{ai}` folder is dropped).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tgsort_core::{AiChoice, Config, Plan};

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const RECHECK_AFTER: Duration = Duration::from_secs(30 * 60);
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// Files per helper call; keeps each call's runtime (and timeout) bounded.
const CHUNK: usize = 16;
/// Extra timeout allowance per file in a chunk, on top of `ai_timeout_secs`.
const SECS_PER_FILE: u64 = 5;
const POLL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(String),
}

#[derive(Serialize)]
struct Request<'a> {
    files: Vec<RequestFile>,
    choices: &'a [AiChoice],
}

#[derive(Serialize)]
struct RequestFile {
    id: String,
    path: String,
    name: String,
}

#[derive(Deserialize)]
struct Response {
    results: Vec<ResponseItem>,
}

#[derive(Deserialize)]
struct ResponseItem {
    id: String,
    choice: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct CheckResponse {
    available: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// The configured helper, or `tgsort-ai` next to the running executable.
pub fn helper_path(cfg: &Config) -> PathBuf {
    if let Some(p) = &cfg.ai_helper {
        return p.clone();
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("tgsort-ai")))
        .unwrap_or_else(|| PathBuf::from("tgsort-ai"))
}

/// Runs `helper args`, feeding `input` to stdin; returns stdout. Kills the helper at `timeout`.
fn run_helper(
    helper: &Path,
    args: &[&str],
    input: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut child = Command::new(helper)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {}", helper.display()))?;
    let deadline = Instant::now() + timeout;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        // Separate thread so a helper that never reads cannot block us past the timeout.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let out = child.stdout.take().context("no stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = out.take(MAX_OUTPUT).read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(r);
    });
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(POLL);
    };
    ensure!(status.success(), "helper exited with {status}");
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(200));
    let out = rx
        .recv_timeout(left)
        .context("helper output not closed in time")?;
    Ok(out?)
}

/// Runs `helper --check`.
pub fn check(helper: &Path) -> Availability {
    let r = run_helper(helper, &["--check"], None, CHECK_TIMEOUT).and_then(|out| {
        serde_json::from_slice::<CheckResponse>(&out).context("unreadable --check output")
    });
    match r {
        Ok(c) if c.available => Availability::Available,
        Ok(c) => Availability::Unavailable(c.reason.unwrap_or_else(|| "unknown reason".into())),
        Err(e) => Availability::Unavailable(format!("{e:#}")),
    }
}

fn try_classify(
    cfg: &Config,
    batch: &[(String, PathBuf)],
    choices: &[AiChoice],
) -> Result<HashMap<String, Option<String>>> {
    let req = Request {
        files: batch
            .iter()
            .map(|(id, p)| RequestFile {
                id: id.clone(),
                path: p.to_string_lossy().into_owned(),
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            })
            .collect(),
        choices,
    };
    // `ai_timeout_secs` is the base; each file in the chunk adds a few seconds.
    let timeout = Duration::from_secs(cfg.ai_timeout_secs + SECS_PER_FILE * batch.len() as u64);
    let out = run_helper(
        &helper_path(cfg),
        &["classify"],
        Some(serde_json::to_vec(&req)?),
        timeout,
    )?;
    let resp: Response = serde_json::from_slice(&out).context("unreadable classify output")?;
    let mut map: HashMap<String, Option<String>> =
        batch.iter().map(|(id, _)| (id.clone(), None)).collect();
    for item in resp.results {
        let Some(slot) = map.get_mut(&item.id) else {
            continue;
        };
        match item.choice {
            // Only exact choice names count; anything else is "unsure".
            Some(c) if choices.iter().any(|x| x.name == c) => *slot = Some(c),
            Some(c) => debug!("ai: ignoring unknown choice {c:?}"),
            None => {
                if let Some(e) = item.error {
                    debug!("ai: no choice for id {}: {e}", item.id);
                }
            }
        }
    }
    Ok(map)
}

/// Asks the helper to pick one of `choices` per file, in chunks of at most 16 files per helper
/// call (timeout per call = `ai_timeout_secs` + 5s per file). Any failure yields `None` for the
/// affected chunk's ids.
pub fn classify(
    cfg: &Config,
    batch: &[(String, PathBuf)],
    choices: &[AiChoice],
) -> HashMap<String, Option<String>> {
    let mut out = HashMap::new();
    for chunk in batch.chunks(CHUNK) {
        match try_classify(cfg, chunk, choices) {
            Ok(m) => out.extend(m),
            Err(e) => {
                warn!("AI classification failed ({e:#}); files go to the parent folder");
                out.extend(chunk.iter().map(|(id, _)| (id.clone(), None)));
            }
        }
    }
    out
}

/// Cached availability: checked lazily, re-checked at most every 30 min while unavailable.
#[derive(Default)]
pub struct Gate {
    state: Option<(Availability, Instant)>,
}

impl Gate {
    pub fn available(&mut self, cfg: &Config) -> bool {
        self.available_at(cfg, Instant::now())
    }

    fn available_at(&mut self, cfg: &Config, now: Instant) -> bool {
        let stale = match &self.state {
            None => true,
            Some((Availability::Unavailable(_), t)) => now.duration_since(*t) >= RECHECK_AFTER,
            Some((Availability::Available, _)) => false,
        };
        if stale {
            let helper = helper_path(cfg);
            let now_state = check(&helper);
            let changed = self.state.as_ref().map(|(a, _)| a) != Some(&now_state);
            if changed {
                match &now_state {
                    Availability::Available => info!("AI classification: available"),
                    Availability::Unavailable(r) => {
                        info!("AI classification: unavailable: {r} — files go to the parent folder")
                    }
                }
            }
            self.state = Some((now_state, now));
        }
        matches!(self.state, Some((Availability::Available, _)))
    }
}

/// Picks a choice for each plan (index-aligned). Plans are grouped by their choices list, so
/// the helper runs once per distinct list. Unavailable helper => all `None`, nothing spawned.
pub fn resolve(cfg: &Config, gate: &mut Gate, plans: &[&Plan]) -> Vec<Option<String>> {
    let mut out = vec![None; plans.len()];
    if plans.is_empty() || !gate.available(cfg) {
        return out;
    }
    let mut groups: Vec<(&[AiChoice], Vec<usize>)> = Vec::new();
    for (i, p) in plans.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|(c, _)| *c == p.ai_choices.as_slice())
        {
            Some((_, idx)) => idx.push(i),
            None => groups.push((p.ai_choices.as_slice(), vec![i])),
        }
    }
    for (choices, idx) in groups {
        let batch: Vec<(String, PathBuf)> = idx
            .iter()
            .map(|&i| (i.to_string(), plans[i].source.clone()))
            .collect();
        let mut res = classify(cfg, &batch, choices);
        for &i in &idx {
            out[i] = res.remove(&i.to_string()).flatten();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-ai");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn cfg_for(helper: &Path, timeout: u64) -> Config {
        Config::parse(&format!(
            "ai_helper = '{}'\nai_timeout_secs = {timeout}",
            helper.display()
        ))
        .unwrap()
    }

    fn choices() -> Vec<AiChoice> {
        vec![
            AiChoice {
                name: "Papers".into(),
                hint: "p".into(),
            },
            AiChoice {
                name: "Books".into(),
                hint: "b".into(),
            },
        ]
    }

    fn batch() -> Vec<(String, PathBuf)> {
        vec![
            ("0".into(), PathBuf::from("/x/a.pdf")),
            ("1".into(), PathBuf::from("/x/b.pdf")),
            ("2".into(), PathBuf::from("/x/c.pdf")),
        ]
    }

    #[test]
    fn request_serialization_matches_protocol() {
        let ch = choices();
        let req = Request {
            files: vec![RequestFile {
                id: "0".into(),
                path: "/x/a.pdf".into(),
                name: "a.pdf".into(),
            }],
            choices: &ch,
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "files": [{"id": "0", "path": "/x/a.pdf", "name": "a.pdf"}],
                "choices": [{"name": "Papers", "hint": "p"}, {"name": "Books", "hint": "b"}]
            })
        );
    }

    #[test]
    fn response_deserialization() {
        let r: Response = serde_json::from_str(
            r#"{"results":[{"id":"0","choice":"Papers"},{"id":"1","choice":null,"error":"boom"}]}"#,
        )
        .unwrap();
        assert_eq!(r.results[0].choice.as_deref(), Some("Papers"));
        assert_eq!(r.results[1].choice, None);
        assert_eq!(r.results[1].error.as_deref(), Some("boom"));
    }

    #[test]
    fn classify_maps_results_and_rejects_unknown_choices() {
        let d = tempfile::tempdir().unwrap();
        // Also proves the request really arrives on stdin.
        let h = fake(
            d.path(),
            r#"grep -q '"choices"' || exit 3
echo '{"results":[{"id":"0","choice":"Books"},{"id":"1","choice":null,"error":"x"},{"id":"2","choice":"Nonsense"},{"id":"zz","choice":"Papers"}]}'"#,
        );
        let r = classify(&cfg_for(&h, 10), &batch(), &choices());
        assert_eq!(r.len(), 3);
        assert_eq!(r["0"].as_deref(), Some("Books"));
        assert_eq!(r["1"], None);
        assert_eq!(r["2"], None);
    }

    #[test]
    fn missing_results_become_none() {
        let d = tempfile::tempdir().unwrap();
        let h = fake(d.path(), r#"cat >/dev/null; echo '{"results":[]}'"#);
        let r = classify(&cfg_for(&h, 10), &batch(), &choices());
        assert!(!r.is_empty() && r.values().all(|v| v.is_none()));
    }

    fn all_none(r: &HashMap<String, Option<String>>) -> bool {
        !r.is_empty() && r.values().all(|v| v.is_none())
    }

    #[test]
    fn nonzero_exit_garbage_missing_and_timeout_fall_back() {
        let d = tempfile::tempdir().unwrap();
        let h = fake(d.path(), "cat >/dev/null; exit 2");
        assert!(all_none(&classify(&cfg_for(&h, 10), &batch(), &choices())));
        let h = fake(d.path(), "cat >/dev/null; echo 'not json {{'");
        assert!(all_none(&classify(&cfg_for(&h, 10), &batch(), &choices())));
        let missing = d.path().join("nope");
        assert!(all_none(&classify(
            &cfg_for(&missing, 10),
            &batch(),
            &choices()
        )));
        let h = fake(d.path(), "exec sleep 30");
        let t = Instant::now();
        assert!(all_none(&classify(
            &cfg_for(&h, 1),
            &batch()[..1],
            &choices()
        )));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn large_batches_are_chunked_and_timeout_scales_with_chunk_size() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("calls");
        // Echoes a choice for every id it sees, and takes 2s per call.
        let h = fake(
            d.path(),
            &format!(
                r#"echo call >> {}
req=$(cat)
sleep 2
ids=$(echo "$req" | grep -o '"id":"[0-9]*"' | sed 's/"id"://')
out=""
for i in $ids; do out="$out{{\"id\":$i,\"choice\":\"Books\"}},"; done
echo "{{\"results\":[${{out%,}}]}}""#,
                log.display()
            ),
        );
        let big: Vec<(String, PathBuf)> = (0..40)
            .map(|i| (i.to_string(), PathBuf::from(format!("/x/{i}.pdf"))))
            .collect();
        // Base timeout 1s < 2s runtime: only the per-file allowance lets each call finish.
        let r = classify(&cfg_for(&h, 1), &big, &choices());
        assert_eq!(r.len(), 40);
        assert!(r.values().all(|v| v.as_deref() == Some("Books")));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 3);
    }

    #[test]
    fn timeout_with_lingering_child_does_not_hang() {
        let d = tempfile::tempdir().unwrap();
        // Non-exec sleep: the grandchild keeps stdout open after the shell is killed.
        let h = fake(d.path(), "sleep 30");
        let t = Instant::now();
        assert!(all_none(&classify(
            &cfg_for(&h, 1),
            &batch()[..1],
            &choices()
        )));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    #[test]
    fn check_parses_availability() {
        let d = tempfile::tempdir().unwrap();
        let h = fake(d.path(), r#"echo '{"available": true}'"#);
        assert_eq!(check(&h), Availability::Available);
        let h = fake(
            d.path(),
            r#"echo '{"available": false, "reason": "appleIntelligenceNotEnabled"}'"#,
        );
        assert_eq!(
            check(&h),
            Availability::Unavailable("appleIntelligenceNotEnabled".into())
        );
        let h = fake(d.path(), "echo garbage");
        assert!(matches!(check(&h), Availability::Unavailable(_)));
        let h = fake(d.path(), "exit 1");
        assert!(matches!(check(&h), Availability::Unavailable(_)));
        assert!(matches!(
            check(&d.path().join("missing")),
            Availability::Unavailable(_)
        ));
    }

    #[test]
    fn gate_caches_and_rechecks_only_when_unavailable() {
        let d = tempfile::tempdir().unwrap();
        let counter = d.path().join("count");
        let h = fake(
            d.path(),
            &format!(
                r#"echo x >> {}; echo '{{"available": false, "reason": "r"}}'"#,
                counter.display()
            ),
        );
        let cfg = cfg_for(&h, 10);
        let calls = || std::fs::read_to_string(&counter).unwrap().lines().count();
        let mut g = Gate::default();
        let t0 = Instant::now();
        assert!(!g.available_at(&cfg, t0));
        assert!(!g.available_at(&cfg, t0 + Duration::from_secs(60)));
        assert_eq!(calls(), 1);
        assert!(!g.available_at(&cfg, t0 + RECHECK_AFTER));
        assert_eq!(calls(), 2);
        // Once available it is never re-checked.
        let h = fake(d.path(), r#"echo '{"available": true}'"#);
        let mut g = Gate::default();
        assert!(g.available_at(&cfg_for(&h, 10), t0));
        assert!(g.available_at(&cfg_for(&d.path().join("gone"), 10), t0 + RECHECK_AFTER * 5));
    }

    #[test]
    fn resolve_groups_by_choices_and_skips_spawn_when_unavailable() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("calls");
        let h = fake(
            d.path(),
            &format!(
                r#"if [ "$1" = "--check" ]; then echo '{{"available": true}}'; exit 0; fi
echo call >> {}
cat >/dev/null
echo '{{"results":[{{"id":"0","choice":"A"}},{{"id":"1","choice":"A"}},{{"id":"2","choice":"A"}}]}}'"#,
                log.display()
            ),
        );
        let cfg = Config::parse(&format!(
            "ai_helper = '{}'\n\
             [[rule]]\nname='R1'\nextensions=['pdf']\ndest='/t/{{ai}}'\nai_choices=[{{name='A'}}]\n\
             [[rule]]\nname='R2'\nextensions=['txt']\ndest='/u/{{ai}}'\nai_choices=[{{name='B'}}]",
            h.display()
        ))
        .unwrap();
        let mk = |rule: usize, src: &str| Plan {
            rule: "r".into(),
            source: PathBuf::from(src),
            dest: PathBuf::from("/t/x"),
            ai_choices: cfg.rules[rule].ai_choices.clone(),
        };
        let (a, b, c) = (mk(0, "/x/a.pdf"), mk(0, "/x/b.pdf"), mk(1, "/x/c.txt"));
        let mut gate = Gate::default();
        let r = resolve(&cfg, &mut gate, &[&a, &c, &b]);
        // R1 group = plans 0 and 2 -> A; R2 group's only choice is B, so "A" is rejected.
        assert_eq!(r[0].as_deref(), Some("A"));
        assert_eq!(r[1], None);
        assert_eq!(r[2].as_deref(), Some("A"));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);

        let off = fake(d.path(), r#"echo '{"available": false, "reason": "r"}'"#);
        let mut cfg2 = cfg.clone();
        cfg2.ai_helper = Some(off);
        let _ = std::fs::remove_file(&log);
        let r = resolve(&cfg2, &mut Gate::default(), &[&a, &b]);
        assert_eq!(r, vec![None, None]);
        assert!(!log.exists());
    }
}
