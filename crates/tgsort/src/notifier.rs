use std::collections::BTreeMap;
use std::process::{Command, Stdio};

const MAX_INDIVIDUAL: usize = 3;

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn script(text: &str) -> String {
    format!(
        "display notification \"{}\" with title \"tgsort\"",
        escape(text)
    )
}

/// Notification texts for a batch of moved `(file, rule)` pairs: one each for up to
/// three files, otherwise a single summary.
pub fn batch(moved: &[(String, String)]) -> Vec<String> {
    if moved.len() <= MAX_INDIVIDUAL {
        return moved.iter().map(|(f, r)| format!("{f} → {r}")).collect();
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, r) in moved {
        *counts.entry(r).or_default() += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let parts: Vec<String> = counts
        .iter()
        .take(4)
        .map(|(r, n)| format!("{n} {r}"))
        .collect();
    let more = if counts.len() > 4 { ", …" } else { "" };
    vec![format!(
        "Moved {} files: {}{more}",
        moved.len(),
        parts.join(", ")
    )]
}

/// Fire-and-forget macOS notifications; failures are ignored.
pub fn notify_all(moved: &[(String, String)]) {
    for text in batch(moved) {
        let script = script(&text);
        std::thread::spawn(move || {
            let _ = Command::new("osascript")
                .args(["-e", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(rules: &[&str]) -> Vec<(String, String)> {
        rules
            .iter()
            .enumerate()
            .map(|(i, r)| (format!("f{i}"), r.to_string()))
            .collect()
    }

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(
            script("x\".zip"),
            "display notification \"x\\\".zip\" with title \"tgsort\""
        );
    }

    #[test]
    fn batching_decision() {
        assert!(batch(&[]).is_empty());
        assert_eq!(
            batch(&items(&["A", "B", "A"])),
            vec!["f0 → A", "f1 → B", "f2 → A"]
        );
        assert_eq!(
            batch(&items(&["Videos", "Docs", "Videos", "Videos"])),
            vec!["Moved 4 files: 3 Videos, 1 Docs"]
        );
        assert_eq!(
            batch(&items(&["a", "b", "c", "d", "e", "f"]))[0],
            "Moved 6 files: 1 a, 1 b, 1 c, 1 d, …"
        );
    }
}
