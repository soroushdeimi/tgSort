use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::rules::{check_template, dest_uses_ai};
use crate::AiChoice;

#[derive(Debug, Clone)]
pub struct Config {
    /// Watched directories (derived from `sources` when those are used).
    pub watch_dirs: Vec<PathBuf>,
    /// `[[source]]` entries; empty in legacy `watch_dirs` mode (then `{root}` is invalid).
    pub sources: Vec<Source>,
    pub settle_secs: u64,
    pub notify: bool,
    pub delete_duplicates: bool,
    pub rules: Vec<Rule>,
    pub fallback_dest: Option<String>,
    /// Master switch for on-device AI classification.
    pub ai_enabled: bool,
    /// Explicit helper path (`~` expanded). `None` = `tgsort-ai` next to the tgsort executable.
    pub ai_helper: Option<PathBuf>,
    pub ai_timeout_secs: u64,
}

/// A watched directory with the destination root that `{root}` expands to for its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub dir: PathBuf,
    pub root: PathBuf,
}

impl Config {
    /// The source whose directory is exactly the parent of `file` (watching is non-recursive,
    /// so a nested source dir never claims files of its parent, and vice versa).
    pub fn source_for(&self, file: &Path) -> Option<&Source> {
        let parent = file.parent()?;
        let parent = parent.canonicalize().unwrap_or_else(|_| parent.into());
        self.sources
            .iter()
            .find(|s| s.dir.canonicalize().unwrap_or_else(|_| s.dir.clone()) == parent)
    }

    /// True if any rule has a `{ai}` destination segment.
    pub fn uses_ai(&self) -> bool {
        self.rules.iter().any(|r| !r.ai_choices.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    /// Lowercase, without the dot. Empty = any extension.
    pub extensions: Vec<String>,
    pub name_regex: Option<Regex>,
    pub min_size_mb: Option<f64>,
    pub max_size_mb: Option<f64>,
    /// Destination directory template.
    pub dest: String,
    /// Choices for the `{ai}` placeholder, in config order. Empty = rule doesn't use AI.
    pub ai_choices: Vec<AiChoice>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    ai_enabled: bool,
    ai_helper: Option<String>,
    ai_timeout_secs: u64,
    watch_dirs: Option<Vec<String>>,
    source: Vec<RawSource>,
    settle_secs: u64,
    notify: bool,
    delete_duplicates: bool,
    fallback_dest: Option<String>,
    rule: Vec<RawRule>,
}

impl Default for Raw {
    fn default() -> Self {
        Raw {
            watch_dirs: None,
            source: Vec::new(),
            settle_secs: 15,
            notify: true,
            delete_duplicates: false,
            fallback_dest: None,
            ai_enabled: true,
            ai_helper: None,
            ai_timeout_secs: 60,
            rule: Vec::new(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawSource {
    dir: String,
    root: String,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawRule {
    name: String,
    extensions: Vec<String>,
    name_regex: Option<String>,
    min_size_mb: Option<f64>,
    max_size_mb: Option<f64>,
    dest: Option<String>,
    ai_choices: Vec<RawChoice>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawChoice {
    name: String,
    hint: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in config {}", path.display()))
    }

    /// Parses and validates config text.
    pub fn parse(text: &str) -> Result<Config> {
        let raw: Raw = toml::from_str(text).context("invalid TOML")?;
        ensure!(
            raw.watch_dirs.is_none() || raw.source.is_empty(),
            "use either watch_dirs or [[source]], not both"
        );
        let abs = |what: &str, d: &str| -> Result<PathBuf> {
            let p = expand_tilde(d)?;
            ensure!(
                p.is_absolute(),
                "{what} {d:?} must be absolute or start with ~"
            );
            Ok(p)
        };
        let sources = raw
            .source
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (|| {
                    ensure!(!s.dir.trim().is_empty(), "missing dir");
                    ensure!(!s.root.trim().is_empty(), "missing root");
                    Ok(Source {
                        dir: abs("dir", &s.dir)?,
                        root: abs("root", &s.root)?,
                    })
                })()
                .with_context(|| format!("source #{}", i + 1))
            })
            .collect::<Result<Vec<_>>>()?;
        let watch_dirs = if sources.is_empty() {
            let wd = raw
                .watch_dirs
                .unwrap_or_else(|| vec!["~/Downloads/Telegram Desktop".into()]);
            ensure!(!wd.is_empty(), "watch_dirs must not be empty");
            wd.iter()
                .map(|d| abs("watch dir", d))
                .collect::<Result<Vec<_>>>()?
        } else {
            sources.iter().map(|s| s.dir.clone()).collect()
        };
        let roots = !sources.is_empty();
        if let Some(f) = &raw.fallback_dest {
            check_template(f, None, false, roots).context("fallback_dest")?;
        }
        ensure!(raw.ai_timeout_secs > 0, "ai_timeout_secs must be positive");
        let ai_helper = raw
            .ai_helper
            .as_deref()
            .map(|h| {
                let p = expand_tilde(h)?;
                ensure!(
                    p.is_absolute(),
                    "ai_helper {h:?} must be absolute or start with ~"
                );
                Ok(p)
            })
            .transpose()?;
        let rules = raw
            .rule
            .into_iter()
            .enumerate()
            .map(|(i, r)| Rule::build(i, r, roots))
            .collect::<Result<Vec<_>>>()?;
        Ok(Config {
            watch_dirs,
            sources,
            settle_secs: raw.settle_secs,
            notify: raw.notify,
            delete_duplicates: raw.delete_duplicates,
            rules,
            fallback_dest: raw.fallback_dest,
            ai_enabled: raw.ai_enabled,
            ai_helper,
            ai_timeout_secs: raw.ai_timeout_secs,
        })
    }

    /// `~/.config/tgsort/config.toml`
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config/tgsort/config.toml")
    }

    pub fn example_toml() -> &'static str {
        include_str!("../../../config.example.toml")
    }
}

impl Rule {
    fn build(idx: usize, r: RawRule, roots: bool) -> Result<Rule> {
        let label = if r.name.is_empty() {
            format!("#{}", idx + 1)
        } else {
            r.name.clone()
        };
        Self::build_inner(r, roots).with_context(|| format!("rule {label}"))
    }

    fn build_inner(r: RawRule, roots: bool) -> Result<Rule> {
        ensure!(!r.name.trim().is_empty(), "missing name");
        let dest = r.dest.filter(|d| !d.trim().is_empty());
        let dest = dest.context("missing dest")?;
        let extensions = r
            .extensions
            .iter()
            .map(|e| {
                let e = e.trim().trim_start_matches('.').to_lowercase();
                ensure!(!e.is_empty(), "empty extension");
                Ok(e)
            })
            .collect::<Result<Vec<_>>>()?;
        let name_regex = r
            .name_regex
            .as_deref()
            .map(|s| Regex::new(s).with_context(|| format!("invalid name_regex {s:?}")))
            .transpose()?;
        for (k, v) in [
            ("min_size_mb", r.min_size_mb),
            ("max_size_mb", r.max_size_mb),
        ] {
            if let Some(v) = v {
                ensure!(
                    v.is_finite() && v >= 0.0,
                    "{k} must be a non-negative number"
                );
            }
        }
        if let (Some(lo), Some(hi)) = (r.min_size_mb, r.max_size_mb) {
            ensure!(lo <= hi, "min_size_mb is greater than max_size_mb");
        }
        ensure!(
            !extensions.is_empty()
                || name_regex.is_some()
                || r.min_size_mb.is_some()
                || r.max_size_mb.is_some(),
            "needs at least one matcher (extensions, name_regex, min_size_mb, max_size_mb)"
        );
        let ai_choices = build_choices(r.ai_choices)?;
        if ai_choices.is_empty() {
            ensure!(
                !dest.contains("{ai}"),
                "dest uses {{ai}} but the rule has no ai_choices"
            );
        }
        check_template(&dest, name_regex.as_ref(), !ai_choices.is_empty(), roots)
            .context("dest")?;
        // A rule whose dest has no {ai} segment never asks the AI.
        let ai_choices = if dest_uses_ai(&dest) {
            ai_choices
        } else {
            Vec::new()
        };
        Ok(Rule {
            name: r.name,
            extensions,
            name_regex,
            min_size_mb: r.min_size_mb,
            max_size_mb: r.max_size_mb,
            dest,
            ai_choices,
        })
    }
}

fn build_choices(raw: Vec<RawChoice>) -> Result<Vec<AiChoice>> {
    let mut seen = std::collections::HashSet::new();
    raw.into_iter()
        .map(|c| {
            let name = c.name;
            ensure!(
                !name.trim().is_empty() && name == name.trim(),
                "ai_choices name {name:?} must be non-empty without surrounding spaces"
            );
            ensure!(
                !name.contains(['/', '\\', '\0']) && name != "." && name != "..",
                "ai_choices name {name:?} is not a safe folder name"
            );
            // Case-insensitive: macOS volumes usually are.
            ensure!(
                seen.insert(name.to_lowercase()),
                "duplicate ai_choices name {name:?}"
            );
            Ok(AiChoice { name, hint: c.hint })
        })
        .collect()
}

/// Expands a leading `~` or `~/` to the home directory.
pub(crate) fn expand_tilde(s: &str) -> Result<PathBuf> {
    let rest = if s == "~" {
        Some("")
    } else {
        s.strip_prefix("~/")
    };
    match rest {
        Some(r) => Ok(dirs::home_dir()
            .context("cannot determine home directory")?
            .join(r)),
        None => Ok(PathBuf::from(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        format!("{:#}", Config::parse(text).unwrap_err())
    }

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(c.watch_dirs, vec![home.join("Downloads/Telegram Desktop")]);
        assert_eq!(c.settle_secs, 15);
        assert!(c.notify);
        assert!(!c.delete_duplicates);
        assert!(c.rules.is_empty());
        assert!(c.fallback_dest.is_none());
    }

    #[test]
    fn full_parse() {
        let c = Config::parse(
            r#"
            watch_dirs = ["/tmp/a", "~/b"]
            settle_secs = 9
            notify = false
            delete_duplicates = true
            fallback_dest = "/tmp/other/{year}"
            [[rule]]
            name = "V"
            extensions = [".MP4", "mkv"]
            min_size_mb = 1
            max_size_mb = 2.5
            dest = "/tmp/v"
            [[rule]]
            name = "S"
            name_regex = '(?i)S\d{2}E\d{2}'
            dest = "~/s"
            "#,
        )
        .unwrap();
        assert_eq!(c.watch_dirs[0], PathBuf::from("/tmp/a"));
        assert_eq!(c.watch_dirs[1], dirs::home_dir().unwrap().join("b"));
        assert_eq!(
            (c.settle_secs, c.notify, c.delete_duplicates),
            (9, false, true)
        );
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].extensions, ["mp4", "mkv"]);
        assert_eq!(c.rules[0].max_size_mb, Some(2.5));
        assert!(c.rules[1].name_regex.is_some());
    }

    #[test]
    fn example_is_valid() {
        let c = Config::parse(Config::example_toml()).unwrap();
        assert!(!c.rules.is_empty());
        assert!(c.fallback_dest.is_some());
        let docs = c.rules.iter().find(|r| r.name == "Documents").unwrap();
        assert_eq!(docs.ai_choices.len(), 7);
        assert!(docs.dest.starts_with("{root}/") && docs.dest.ends_with("/{ai}"));
        assert_eq!(c.sources.len(), 2);
        assert!(c.rules.iter().any(|r| r.name == "Installers"));
    }

    #[test]
    fn ai_defaults_and_parse() {
        let c = Config::parse("").unwrap();
        assert!(c.ai_enabled && c.ai_helper.is_none() && c.ai_timeout_secs == 60);
        assert!(!c.uses_ai());
        let c = Config::parse(
            "ai_enabled = false\nai_helper = '~/bin/h'\nai_timeout_secs = 5\n\
             [[rule]]\nname='D'\nextensions=['pdf']\ndest='/t/{ext}/{ai}'\n\
             ai_choices=[{name='B', hint='b'},{name='A'}]",
        )
        .unwrap();
        assert!(!c.ai_enabled && c.ai_timeout_secs == 5 && c.uses_ai());
        assert_eq!(
            c.ai_helper.unwrap(),
            dirs::home_dir().unwrap().join("bin/h")
        );
        let ch = &c.rules[0].ai_choices;
        assert_eq!(ch.len(), 2);
        assert_eq!((ch[0].name.as_str(), ch[0].hint.as_str()), ("B", "b"));
        assert_eq!((ch[1].name.as_str(), ch[1].hint.as_str()), ("A", ""));
    }

    #[test]
    fn ai_validation_errors() {
        let r = |dest: &str, choices: &str| {
            err(&format!(
                "[[rule]]\nname='a'\nextensions=['x']\ndest='{dest}'\nai_choices={choices}"
            ))
        };
        assert!(r("/t/{ai}", "[]").contains("no ai_choices"));
        assert!(r("/t/x{ai}", "[{name='A'}]").contains("entire path segment"));
        assert!(r("/t/{ai}/{ai}x", "[{name='A'}]").contains("entire path segment"));
        assert!(r("{ai}", "[{name='A'}]").contains("absolute"));
        assert!(r("/{ai}", "[{name='A'}]").contains("absolute"));
        for bad in ["''", "'a/b'", "'.'", "'..'", "' x'"] {
            assert!(
                r("/t/{ai}", &format!("[{{name={bad}}}]")).contains("ai_choices"),
                "{bad}"
            );
        }
        assert!(r("/t/{ai}", "[{name='A'},{name='a'}]").contains("duplicate"));
        assert!(r("/t/{ai}", "[{name='A',bogus=1}]").contains("bogus"));
        assert!(err("fallback_dest='/t/{ai}'").contains("placeholder {ai}"));
        assert!(err("ai_timeout_secs = 0").contains("ai_timeout_secs"));
        assert!(err("ai_helper = 'rel/h'").contains("ai_helper"));
        // choices without {ai} in dest are accepted but inert
        let c = Config::parse(
            "[[rule]]\nname='a'\nextensions=['x']\ndest='/t'\nai_choices=[{name='A'}]",
        )
        .unwrap();
        assert!(!c.uses_ai());
    }

    #[test]
    fn sources_and_root_validation() {
        let c = Config::parse(
            "fallback_dest='{root}/Other/{ext}'\n\
             [[source]]\ndir='/tmp/d'\nroot='~/R'\n[[source]]\ndir='/tmp/d/t'\nroot='/r2'\n\
             [[rule]]\nname='a'\nextensions=['x']\ndest='{root}'",
        )
        .unwrap();
        assert_eq!(c.sources.len(), 2);
        assert_eq!(c.sources[0].root, dirs::home_dir().unwrap().join("R"));
        assert_eq!(
            c.watch_dirs,
            [PathBuf::from("/tmp/d"), PathBuf::from("/tmp/d/t")]
        );
        let rule = |d: &str| format!("[[rule]]\nname='a'\nextensions=['x']\ndest='{d}'");
        let src = "[[source]]\ndir='/d'\nroot='/r'\n";
        assert!(err(&format!("{src}{}", rule("/t/{root}"))).contains("{root}"));
        assert!(err(&format!("{src}{}", rule("{root}x/a"))).contains("{root}"));
        assert!(err(&format!("{src}{}", rule("{root}/{root}"))).contains("{root}"));
        assert!(err(&rule("{root}/a")).contains("{root}"));
        assert!(err("fallback_dest='{root}/a'").contains("{root}"));
        assert!(err(&format!("watch_dirs=['/a']\n{src}")).contains("not both"));
        assert!(err("[[source]]\ndir='/d'").contains("missing root"));
        assert!(err("[[source]]\nroot='/d'").contains("missing dir"));
        assert!(err("[[source]]\ndir='rel'\nroot='/d'").contains("absolute"));
        assert!(err("[[source]]\ndir='/d'\nroot='rel'").contains("absolute"));
        // legacy mode still works
        assert!(Config::parse("watch_dirs=['/a']")
            .unwrap()
            .sources
            .is_empty());
    }

    #[test]
    fn default_path_ends_correctly() {
        assert!(Config::default_path().ends_with(".config/tgsort/config.toml"));
    }

    #[test]
    fn load_reports_path() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.toml");
        assert!(format!("{:#}", Config::load(&p).unwrap_err()).contains("c.toml"));
        std::fs::write(&p, "settle_secs = 1").unwrap();
        assert_eq!(Config::load(&p).unwrap().settle_secs, 1);
    }

    #[test]
    fn validation_errors() {
        assert!(err("[[rule]]\nname='a'\nextensions=['x']").contains("missing dest"));
        assert!(err("[[rule]]\nname='a'\ndest='/t'").contains("at least one matcher"));
        assert!(err("[[rule]]\nextensions=['x']\ndest='/t'").contains("missing name"));
        assert!(err("[[rule]]\nname='a'\nname_regex='('\ndest='/t'").contains("invalid name_regex"));
        assert!(
            err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t/{nope}'")
                .contains("unknown or empty placeholder {nope}")
        );
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t/{year'").contains("unclosed"));
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='rel/dir'").contains("absolute"));
        assert!(
            err("[[rule]]\nname='a'\nmin_size_mb=5\nmax_size_mb=1\ndest='/t'")
                .contains("greater than")
        );
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t'\nbogus=1").contains("bogus"));
        assert!(
            err("[[rule]]\nname='a'\nname_regex='(?P<year>.)'\ndest='/t'").contains("collides")
        );
        assert!(err("fallback_dest = '/t/{series}'").contains("fallback_dest"));
        assert!(err("watch_dirs = []").contains("watch_dirs"));
        assert!(err("watch_dirs = ['relative']").contains("absolute"));
        assert!(err("settle_secs = 'x'").contains("invalid TOML"));
    }

    #[test]
    fn named_group_placeholder_is_accepted() {
        let t = "[[rule]]\nname='a'\nname_regex='(?P<series>.+)S01'\ndest='/t/{series}'";
        assert!(Config::parse(t).is_ok());
    }
}
