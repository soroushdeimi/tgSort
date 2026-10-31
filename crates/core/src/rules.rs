use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Datelike, Local};
use regex::Regex;
use serde::Serialize;

use crate::config::expand_tilde;
use crate::{Config, Rule};

const BUILTINS: [&str; 5] = ["year", "month", "day", "ext", "stem"];
const TEMP_EXTS: [&str; 7] = [
    "part",
    "partial",
    "tmp",
    "temp",
    "crdownload",
    "download",
    "!qb",
];
const MB: f64 = 1024.0 * 1024.0;

/// One candidate folder for the `{ai}` placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AiChoice {
    pub name: String,
    pub hint: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub rule: String,
    pub source: PathBuf,
    /// Full target file path.
    pub dest: PathBuf,
    /// Choices for the rule's `{ai}` segment; empty when the rule doesn't use AI.
    /// `dest` is then the fallback (segment dropped) unless planned with a choice.
    pub ai_choices: Vec<AiChoice>,
}

const ROOT: &str = "{root}";

/// `(starts_with_root, remainder_without_leading_slash)`.
fn split_root(tpl: &str) -> (bool, &str) {
    match tpl.strip_prefix(ROOT) {
        Some("") => (true, ""),
        Some(r) if r.starts_with('/') => (true, r.trim_start_matches('/')),
        _ => (false, tpl),
    }
}

const AI_SEGMENT: &str = "{ai}";

/// True if the template has a `{ai}` path segment.
pub(crate) fn dest_uses_ai(tpl: &str) -> bool {
    tpl.split('/').any(|s| s == AI_SEGMENT)
}

/// Drops every `{ai}` segment (keeping it for `keep`, to be rendered later).
fn drop_ai(tpl: &str) -> String {
    tpl.split('/')
        .filter(|s| *s != AI_SEGMENT)
        .collect::<Vec<_>>()
        .join("/")
}

/// Hidden files, `.DS_Store`, directories and partial/temporary downloads.
pub fn is_ignorable(path: &Path) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
        return true;
    };
    if name.starts_with('.') || name.starts_with("~$") || path.is_dir() {
        return true;
    }
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| TEMP_EXTS.contains(&e.as_str()))
}

/// Decides where `file` should go. `None` = ignorable, or nothing matched and no fallback.
pub fn plan(cfg: &Config, file: &Path) -> Result<Option<Plan>> {
    plan_with_choice(cfg, file, None)
}

/// Like [`plan`], but fills the rule's `{ai}` segment with `choice`. A choice that is not one
/// of the rule's names is treated as `None` (segment dropped).
pub fn plan_with_choice(cfg: &Config, file: &Path, choice: Option<&str>) -> Result<Option<Plan>> {
    if is_ignorable(file) {
        return Ok(None);
    }
    let file_name = file.file_name().context("path has no file name")?;
    let name = file_name.to_string_lossy();
    let meta = fs::metadata(file).with_context(|| format!("cannot stat {}", file.display()))?;
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let (rule, tpl, re, choices) = match cfg
        .rules
        .iter()
        .find(|r| r.matches(&name, &ext, meta.len()))
    {
        Some(r) => (
            r.name.as_str(),
            r.dest.as_str(),
            r.name_regex.as_ref(),
            r.ai_choices.as_slice(),
        ),
        None => match &cfg.fallback_dest {
            Some(d) => ("fallback", d.as_str(), None, &[][..]),
            None => return Ok(None),
        },
    };
    let choice = choice.filter(|c| choices.iter().any(|x| x.name == *c));

    let when: DateTime<Local> = meta.modified().context("no modification time")?.into();
    let mut vars: HashMap<String, String> = HashMap::from([
        ("year".into(), format!("{:04}", when.year())),
        ("month".into(), format!("{:02}", when.month())),
        ("day".into(), format!("{:02}", when.day())),
        (
            "ext".into(),
            if ext.is_empty() { "no-ext".into() } else { ext },
        ),
        (
            "stem".into(),
            file.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    ]);
    if let Some(caps) = re.and_then(|re| re.captures(&name).map(|c| (re, c))) {
        for n in caps.0.capture_names().flatten() {
            if let Some(m) = caps.1.name(n) {
                let v = m.as_str().replace(['.', '_'], " ").trim().to_string();
                if !v.is_empty() {
                    vars.insert(n.into(), v);
                }
            }
        }
    }

    let tpl = match choice {
        Some(c) => {
            vars.insert("ai".into(), c.into());
            tpl.to_string()
        }
        None => drop_ai(tpl),
    };
    let (has_root, rest) = split_root(&tpl);
    let dir = if has_root {
        let src = cfg
            .source_for(file)
            .with_context(|| format!("{} is not in a configured [[source]]", file.display()))?;
        src.root.join(render(rest, &vars)?)
    } else {
        expand_tilde(&render(&tpl, &vars)?)?
    };
    ensure!(
        dir.is_absolute(),
        "destination {} is not absolute",
        dir.display()
    );
    Ok(Some(Plan {
        rule: rule.into(),
        source: file.into(),
        dest: dir.join(file_name),
        ai_choices: choices.to_vec(),
    }))
}

impl Rule {
    pub(crate) fn matches(&self, name: &str, ext: &str, size: u64) -> bool {
        let size = size as f64;
        (self.extensions.is_empty() || self.extensions.iter().any(|e| e == ext))
            && self.name_regex.as_ref().is_none_or(|re| re.is_match(name))
            && self.min_size_mb.is_none_or(|m| size >= m * MB)
            && self.max_size_mb.is_none_or(|m| size <= m * MB)
    }
}

/// Substitutes `{key}` placeholders; values may not be empty, contain `/`, or be `.`/`..`.
pub(crate) fn render(tpl: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        ensure!(
            rest.as_bytes()[i] == b'{',
            "unmatched '}}' in template {tpl:?}"
        );
        let after = &rest[i + 1..];
        let j = after
            .find('}')
            .with_context(|| format!("unclosed '{{' in template {tpl:?}"))?;
        let key = &after[..j];
        let v = vars.get(key).with_context(|| {
            format!("unknown or empty placeholder {{{key}}} in template {tpl:?}")
        })?;
        if v.is_empty() || v.contains(['/', '\0']) || v == ".." || v == "." {
            bail!("unsafe value {v:?} for placeholder {{{key}}}");
        }
        out.push_str(v);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Config-time check: placeholders known (built-ins + named groups) and result absolute.
pub(crate) fn check_template(tpl: &str, re: Option<&Regex>, ai: bool, roots: bool) -> Result<()> {
    if tpl.contains(ROOT) {
        ensure!(
            roots,
            "{{root}} needs [[source]] entries (not watch_dirs) in {tpl:?}"
        );
        ensure!(
            split_root(tpl).0 && !split_root(tpl).1.contains(ROOT),
            "{{root}} must be the first path segment in {tpl:?}"
        );
        let rest = split_root(tpl).1;
        return check_template(&format!("/x/{rest}"), re, ai, false);
    }
    if ai {
        for seg in tpl.split('/') {
            ensure!(
                seg == AI_SEGMENT || !seg.contains(AI_SEGMENT),
                "{{ai}} must be an entire path segment in {tpl:?}"
            );
        }
    }
    let mut vars: HashMap<String, String> = BUILTINS
        .iter()
        .map(|k| (k.to_string(), "x".to_string()))
        .collect();
    for n in re.into_iter().flat_map(|r| r.capture_names().flatten()) {
        ensure!(
            !BUILTINS.contains(&n),
            "capture group {n:?} collides with a built-in placeholder"
        );
        vars.insert(n.into(), "x".into());
    }
    if ai {
        // The fallback (segment dropped) must be valid on its own, too.
        let dropped = drop_ai(tpl);
        let dir = expand_tilde(&render(&dropped, &vars)?)?;
        ensure!(
            dir.is_absolute(),
            "{tpl:?} must stay absolute when {{ai}} is dropped"
        );
        vars.insert("ai".into(), "x".into());
    }
    let dir = expand_tilde(&render(tpl, &vars)?)?;
    ensure!(
        dir.is_absolute(),
        "{tpl:?} must be absolute or start with ~"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, SystemTime};

    fn touch(dir: &Path, name: &str, size: usize) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, vec![b'a'; size]).unwrap();
        p
    }

    fn cfg(toml: &str, root: &Path) -> Config {
        Config::parse(&toml.replace("ROOT", root.to_str().unwrap())).unwrap()
    }

    fn dest_of(c: &Config, f: &Path) -> Option<(String, PathBuf)> {
        plan(c, f).unwrap().map(|p| (p.rule, p.dest))
    }

    #[test]
    fn ignorable() {
        let d = tempfile::tempdir().unwrap();
        for n in [
            ".hidden",
            ".DS_Store",
            "a.part",
            "a.PARTIAL",
            "a.tmp",
            "a.temp",
            "a.crdownload",
            "a.download",
            "a.mp4.!qb",
            "~$doc.docx",
        ] {
            assert!(is_ignorable(&d.path().join(n)), "{n}");
        }
        fs::create_dir(d.path().join("folder.mp4")).unwrap();
        assert!(is_ignorable(&d.path().join("folder.mp4")));
        assert!(is_ignorable(Path::new("/")));
        for n in ["a.mp4", "noext", "tmp", "my~$file.txt"] {
            assert!(!is_ignorable(&d.path().join(n)), "{n}");
        }
    }

    #[test]
    fn extension_match_case_insensitive_and_first_match_wins() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='V'\nextensions=['mp4']\ndest='ROOT/v'\n\
             [[rule]]\nname='All'\nextensions=['mp4','pdf']\ndest='ROOT/all'",
            d.path(),
        );
        let (r, p) = dest_of(&c, &touch(d.path(), "A.MP4", 1)).unwrap();
        assert_eq!((r.as_str(), p), ("V", d.path().join("v/A.MP4")));
        let (r, _) = dest_of(&c, &touch(d.path(), "b.pdf", 1)).unwrap();
        assert_eq!(r, "All");
        assert!(dest_of(&c, &touch(d.path(), "c.txt", 1)).is_none());
        assert!(dest_of(&c, &touch(d.path(), "noext", 1)).is_none());
    }

    #[test]
    fn regex_and_extension_must_both_match() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='S'\nextensions=['mkv']\nname_regex='(?i)s\\d{2}e\\d{2}'\ndest='ROOT/s'",
            d.path(),
        );
        assert!(dest_of(&c, &touch(d.path(), "Show.S01E02.mkv", 1)).is_some());
        assert!(dest_of(&c, &touch(d.path(), "Show.S01E02.mp4", 1)).is_none());
        assert!(dest_of(&c, &touch(d.path(), "Movie.mkv", 1)).is_none());
    }

    #[test]
    fn size_bounds() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='M'\nmin_size_mb=1\nmax_size_mb=2\ndest='ROOT/m'",
            d.path(),
        );
        let mb = 1024 * 1024;
        assert!(dest_of(&c, &touch(d.path(), "small", mb - 1)).is_none());
        assert!(dest_of(&c, &touch(d.path(), "lo", mb)).is_some());
        assert!(dest_of(&c, &touch(d.path(), "hi", 2 * mb)).is_some());
        assert!(dest_of(&c, &touch(d.path(), "big", 2 * mb + 1)).is_none());
    }

    #[test]
    fn fallback_and_ignorable_files() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "fallback_dest='ROOT/other'\n[[rule]]\nname='V'\nextensions=['mp4']\ndest='ROOT/v'",
            d.path(),
        );
        let (r, p) = dest_of(&c, &touch(d.path(), "x.zip", 1)).unwrap();
        assert_eq!((r.as_str(), p), ("fallback", d.path().join("other/x.zip")));
        assert!(dest_of(&c, &touch(d.path(), "x.zip.part", 1)).is_none());
        assert!(plan(&c, &d.path().join("missing.zip")).is_err());
    }

    #[test]
    fn builtin_placeholders() {
        let d = tempfile::tempdir().unwrap();
        let f = touch(d.path(), "My.Clip.MP4", 1);
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(t)
            .unwrap();
        let when: DateTime<Local> = t.into();
        let c = cfg(
            "[[rule]]\nname='V'\nextensions=['mp4']\ndest='ROOT/{year}/{month}/{day}/{ext}/{stem}'",
            d.path(),
        );
        let want = d
            .path()
            .join(when.format("%Y/%m/%d").to_string())
            .join("mp4/My.Clip/My.Clip.MP4");
        assert_eq!(dest_of(&c, &f).unwrap().1, want);
    }

    #[test]
    fn extensionless_file_gets_no_ext_placeholder() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg("fallback_dest='ROOT/other/{ext}'\n[[rule]]\nname='V'\nextensions=['mp4']\ndest='ROOT/v'", d.path());
        let (r, p) = dest_of(&c, &touch(d.path(), "README", 1)).unwrap();
        assert_eq!(
            (r.as_str(), p),
            ("fallback", d.path().join("other/no-ext/README"))
        );
    }

    #[test]
    fn named_capture_groups() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='S'\nname_regex='(?P<series>.+?)[ ._-]S\\d{2}'\ndest='ROOT/{series}'",
            d.path(),
        );
        let (_, p) = dest_of(&c, &touch(d.path(), "Breaking_Bad.S01E01.mkv", 1)).unwrap();
        assert_eq!(p, d.path().join("Breaking Bad/Breaking_Bad.S01E01.mkv"));
    }

    #[test]
    fn unmatched_optional_group_errors_in_plan() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='S'\nname_regex='a(?P<x>b)?'\ndest='ROOT/{x}'",
            d.path(),
        );
        let e = plan(&c, &touch(d.path(), "a.txt", 1)).unwrap_err();
        assert!(format!("{e:#}").contains("{x}"));
    }

    #[test]
    fn tilde_dest_is_expanded() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "[[rule]]\nname='V'\nextensions=['mp4']\ndest='~/Movies/x'",
            d.path(),
        );
        let (_, p) = dest_of(&c, &touch(d.path(), "a.mp4", 1)).unwrap();
        assert_eq!(p, dirs::home_dir().unwrap().join("Movies/x/a.mp4"));
    }

    const AI_CFG: &str = "[[rule]]\nname='Docs'\nextensions=['pdf']\n\
        dest='ROOT/{ext}/{ai}'\nai_choices=[{name='Papers',hint='p'},{name='Books',hint='b'}]";

    #[test]
    fn ai_plan_falls_back_without_segment() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(AI_CFG, d.path());
        let f = touch(d.path(), "a.pdf", 1);
        let p = plan(&c, &f).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("pdf/a.pdf"));
        assert_eq!(p.ai_choices.len(), 2);
        assert_eq!(p.ai_choices[0].name, "Papers");
        // non-AI rules carry no choices
        let c2 = cfg(
            "[[rule]]\nname='V'\nextensions=['pdf']\ndest='ROOT/v'",
            d.path(),
        );
        assert!(plan(&c2, &f).unwrap().unwrap().ai_choices.is_empty());
    }

    #[test]
    fn plan_with_choice_fills_segment() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(AI_CFG, d.path());
        let f = touch(d.path(), "a.pdf", 1);
        let p = plan_with_choice(&c, &f, Some("Books")).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("pdf/Books/a.pdf"));
        assert_eq!(p.ai_choices.len(), 2);
        for bad in [Some("Nope"), Some("books"), Some("../x"), Some(""), None] {
            let p = plan_with_choice(&c, &f, bad).unwrap().unwrap();
            assert_eq!(p.dest, d.path().join("pdf/a.pdf"), "{bad:?}");
        }
    }

    #[test]
    fn ai_segment_in_the_middle_and_fallback_rule() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(
            "fallback_dest='ROOT/other'\n[[rule]]\nname='Docs'\nextensions=['pdf']\n\
             dest='ROOT/{ai}/{ext}'\nai_choices=[{name='P'}]",
            d.path(),
        );
        let f = touch(d.path(), "a.pdf", 1);
        let p = plan_with_choice(&c, &f, Some("P")).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("P/pdf/a.pdf"));
        assert_eq!(
            plan(&c, &f).unwrap().unwrap().dest,
            d.path().join("pdf/a.pdf")
        );
        // choice is ignored for rules without AI (fallback rule)
        let g = touch(d.path(), "b.zip", 1);
        let p = plan_with_choice(&c, &g, Some("P")).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("other/b.zip"));
        assert!(p.ai_choices.is_empty());
    }

    #[test]
    fn root_placeholder_uses_the_files_source() {
        let d = tempfile::tempdir().unwrap();
        let dl = d.path().join("Downloads");
        let tg = dl.join("Telegram Desktop");
        fs::create_dir_all(&tg).unwrap();
        let c = cfg(
            &format!(
                "fallback_dest='{{root}}/Other/{{ext}}'\n\
                 [[source]]\ndir='{}'\nroot='ROOT/tg'\n[[source]]\ndir='{}'\nroot='ROOT/dl'\n\
                 [[rule]]\nname='Docs'\nextensions=['pdf']\ndest='{{root}}/Docs/{{ext}}/{{ai}}'\n\
                 ai_choices=[{{name='P'}}]\n\
                 [[rule]]\nname='Inst'\nextensions=['dmg']\ndest='{{root}}/Installers'",
                tg.display(),
                dl.display()
            ),
            d.path(),
        );
        let (a, b) = (touch(&tg, "a.pdf", 1), touch(&dl, "a.pdf", 1));
        assert_eq!(
            dest_of(&c, &a).unwrap().1,
            d.path().join("tg/Docs/pdf/a.pdf")
        );
        assert_eq!(
            dest_of(&c, &b).unwrap().1,
            d.path().join("dl/Docs/pdf/a.pdf")
        );
        let p = plan_with_choice(&c, &b, Some("P")).unwrap().unwrap();
        assert_eq!(p.dest, d.path().join("dl/Docs/pdf/P/a.pdf"));
        let m = touch(&dl, "x.dmg", 1);
        assert_eq!(
            dest_of(&c, &m).unwrap().1,
            d.path().join("dl/Installers/x.dmg")
        );
        let z = touch(&tg, "z.zip", 1);
        assert_eq!(
            dest_of(&c, &z).unwrap().1,
            d.path().join("tg/Other/zip/z.zip")
        );
        // the nested source dir is a directory in the parent source: never planned/moved
        assert!(plan(&c, &tg).unwrap().is_none());
        // file outside every source cannot resolve {root}
        let out = tempfile::tempdir().unwrap();
        assert!(plan(&c, &touch(out.path(), "o.pdf", 1)).is_err());
        // source_for is an exact parent match
        assert_eq!(c.source_for(&a).unwrap().root, d.path().join("tg"));
        assert_eq!(c.source_for(&b).unwrap().root, d.path().join("dl"));
    }

    #[test]
    fn render_rejects_unsafe_values() {
        let v = |s: &str| HashMap::from([("k".to_string(), s.to_string())]);
        assert_eq!(render("/a/{k}/b", &v("ok")).unwrap(), "/a/ok/b");
        for bad in ["a/b", "..", ".", ""] {
            assert!(render("/a/{k}", &v(bad)).is_err(), "{bad:?}");
        }
        assert!(render("/a/{nope}", &v("x")).is_err());
        assert!(render("/a/}", &v("x")).is_err());
        assert!(render("/a/{k", &v("x")).is_err());
    }
}
