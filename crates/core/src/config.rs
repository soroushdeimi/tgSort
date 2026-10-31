use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::rules::check_template;

#[derive(Debug, Clone)]
pub struct Config {
    pub watch_dirs: Vec<PathBuf>,
    pub settle_secs: u64,
    pub notify: bool,
    pub delete_duplicates: bool,
    pub rules: Vec<Rule>,
    pub fallback_dest: Option<String>,
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
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    watch_dirs: Vec<String>,
    settle_secs: u64,
    notify: bool,
    delete_duplicates: bool,
    fallback_dest: Option<String>,
    rule: Vec<RawRule>,
}

impl Default for Raw {
    fn default() -> Self {
        Raw {
            watch_dirs: vec!["~/Downloads/Telegram Desktop".into()],
            settle_secs: 15,
            notify: true,
            delete_duplicates: false,
            fallback_dest: None,
            rule: Vec::new(),
        }
    }
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
        ensure!(!raw.watch_dirs.is_empty(), "watch_dirs must not be empty");
        let watch_dirs = raw
            .watch_dirs
            .iter()
            .map(|d| {
                let p = expand_tilde(d)?;
                ensure!(p.is_absolute(), "watch dir {d:?} must be absolute or start with ~");
                Ok(p)
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(f) = &raw.fallback_dest {
            check_template(f, None).context("fallback_dest")?;
        }
        let rules = raw
            .rule
            .into_iter()
            .enumerate()
            .map(|(i, r)| Rule::build(i, r))
            .collect::<Result<Vec<_>>>()?;
        Ok(Config {
            watch_dirs,
            settle_secs: raw.settle_secs,
            notify: raw.notify,
            delete_duplicates: raw.delete_duplicates,
            rules,
            fallback_dest: raw.fallback_dest,
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
    fn build(idx: usize, r: RawRule) -> Result<Rule> {
        let label = if r.name.is_empty() { format!("#{}", idx + 1) } else { r.name.clone() };
        Self::build_inner(r).with_context(|| format!("rule {label}"))
    }

    fn build_inner(r: RawRule) -> Result<Rule> {
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
        for (k, v) in [("min_size_mb", r.min_size_mb), ("max_size_mb", r.max_size_mb)] {
            if let Some(v) = v {
                ensure!(v.is_finite() && v >= 0.0, "{k} must be a non-negative number");
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
        check_template(&dest, name_regex.as_ref()).context("dest")?;
        Ok(Rule {
            name: r.name,
            extensions,
            name_regex,
            min_size_mb: r.min_size_mb,
            max_size_mb: r.max_size_mb,
            dest,
        })
    }
}

/// Expands a leading `~` or `~/` to the home directory.
pub(crate) fn expand_tilde(s: &str) -> Result<PathBuf> {
    let rest = if s == "~" { Some("") } else { s.strip_prefix("~/") };
    match rest {
        Some(r) => Ok(dirs::home_dir().context("cannot determine home directory")?.join(r)),
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
        assert_eq!((c.settle_secs, c.notify, c.delete_duplicates), (9, false, true));
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
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t/{nope}'")
            .contains("unknown or empty placeholder {nope}"));
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t/{year'").contains("unclosed"));
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='rel/dir'").contains("absolute"));
        assert!(err("[[rule]]\nname='a'\nmin_size_mb=5\nmax_size_mb=1\ndest='/t'")
            .contains("greater than"));
        assert!(err("[[rule]]\nname='a'\nextensions=['x']\ndest='/t'\nbogus=1").contains("bogus"));
        assert!(err("[[rule]]\nname='a'\nname_regex='(?P<year>.)'\ndest='/t'")
            .contains("collides"));
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
