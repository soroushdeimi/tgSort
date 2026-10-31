mod ai;
mod launchd;
mod notifier;
mod pending;
mod watcher;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tgsort_core::{is_ignorable, plan, plan_with_choice, Config, Plan};

#[derive(Parser)]
#[command(
    version,
    about = "Sort finished Telegram Desktop downloads into folders by rule"
)]
struct Cli {
    /// Config file (default: ~/.config/tgsort/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Debug logging
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write an example config
    Init {
        /// Overwrite an existing config
        #[arg(long)]
        force: bool,
    },
    /// Validate the config and show what would happen to current files (moves nothing)
    Check {
        /// Actually ask the on-device AI helper (still moves nothing)
        #[arg(long)]
        ai: bool,
    },
    /// Process existing settled files once, then exit
    Once {
        #[arg(long)]
        dry_run: bool,
    },
    /// Run the watcher daemon
    Run {
        #[arg(long)]
        dry_run: bool,
    },
    /// Install and load the LaunchAgent
    Install,
    /// Unload and remove the LaunchAgent
    Uninstall,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    let path = cli.config.clone().unwrap_or_else(Config::default_path);

    match cli.cmd {
        Cmd::Init { force } => {
            if path.exists() && !force {
                bail!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                );
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, Config::example_toml())
                .with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
        }
        Cmd::Check { ai } => check(&Config::load(&path)?, ai),
        Cmd::Once { dry_run } => watcher::once(&Config::load(&path)?, dry_run),
        Cmd::Run { dry_run } => watcher::run(&Config::load(&path)?, dry_run)?,
        Cmd::Install => launchd::install(cli.config.as_deref(), &path)?,
        Cmd::Uninstall => launchd::uninstall()?,
    }
    Ok(())
}

fn check(cfg: &Config, use_ai: bool) {
    println!("config OK: {} rule(s)", cfg.rules.len());
    let ai_on = cfg.ai_enabled && cfg.uses_ai();
    let mut gate = ai::Gate::default();
    if use_ai && ai_on {
        println!("AI helper: {}", ai::helper_path(cfg).display());
        gate.available(cfg);
    }
    for dir in &cfg.watch_dirs {
        match cfg.sources.iter().find(|s| s.dir == *dir) {
            Some(s) => println!("\n{}  (root: {})", dir.display(), s.root.display()),
            None => println!("\n{}", dir.display()),
        }
        if !dir.is_dir() {
            println!("  (directory does not exist)");
            continue;
        }
        let files: Vec<_> = watcher::list_files(dir)
            .into_iter()
            .filter(|f| !is_ignorable(f))
            .collect();
        let plans: Vec<_> = files.iter().map(|f| plan(cfg, f)).collect();
        // One classification per distinct choices list, only with --ai.
        let ai_idx: Vec<usize> = (0..plans.len())
            .filter(|&i| ai_on && matches!(&plans[i], Ok(Some(p)) if !p.ai_choices.is_empty()))
            .collect();
        let mut choice = vec![None; plans.len()];
        if use_ai && !ai_idx.is_empty() {
            let refs: Vec<&Plan> = ai_idx
                .iter()
                .filter_map(|&i| plans[i].as_ref().ok().and_then(|p| p.as_ref()))
                .collect();
            for (&i, c) in ai_idx.iter().zip(ai::resolve(cfg, &mut gate, &refs)) {
                choice[i] = c;
            }
        }
        for (i, (f, p)) in files.iter().zip(&plans).enumerate() {
            let name = f.file_name().unwrap_or_default().to_string_lossy();
            match p {
                Ok(Some(p)) if ai_idx.contains(&i) && !use_ai => {
                    let dir = p.dest.parent().unwrap_or(&p.dest).join("{ai?}");
                    println!("  {name} -> {} -> {}/{name}", p.rule, dir.display());
                }
                Ok(Some(p)) if ai_idx.contains(&i) => {
                    let chosen = choice[i].as_deref();
                    match plan_with_choice(cfg, f, chosen) {
                        Ok(Some(q)) => println!(
                            "  {name} -> {} -> {}  [AI: {}]",
                            q.rule,
                            q.dest.display(),
                            chosen.unwrap_or("no choice")
                        ),
                        _ => println!("  {name} -> {} -> {}", p.rule, p.dest.display()),
                    }
                }
                Ok(Some(p)) => println!("  {name} -> {} -> {}", p.rule, p.dest.display()),
                Ok(None) => println!("  {name} -> (no match)"),
                Err(e) => println!("  {name} -> error: {e:#}"),
            }
        }
    }
}
