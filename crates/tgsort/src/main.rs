mod launchd;
mod notifier;
mod pending;
mod watcher;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tgsort_core::{is_ignorable, plan, Config};

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
    Check,
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
        Cmd::Check => check(&Config::load(&path)?),
        Cmd::Once { dry_run } => watcher::once(&Config::load(&path)?, dry_run),
        Cmd::Run { dry_run } => watcher::run(&Config::load(&path)?, dry_run)?,
        Cmd::Install => launchd::install(cli.config.as_deref(), &path)?,
        Cmd::Uninstall => launchd::uninstall()?,
    }
    Ok(())
}

fn check(cfg: &Config) {
    println!("config OK: {} rule(s)", cfg.rules.len());
    for dir in &cfg.watch_dirs {
        println!("\n{}", dir.display());
        if !dir.is_dir() {
            println!("  (directory does not exist)");
            continue;
        }
        for f in watcher::list_files(dir).iter().filter(|f| !is_ignorable(f)) {
            let name = f.file_name().unwrap_or_default().to_string_lossy();
            match plan(cfg, f) {
                Ok(Some(p)) => println!("  {name} -> {} -> {}", p.rule, p.dest.display()),
                Ok(None) => println!("  {name} -> (no match)"),
                Err(e) => println!("  {name} -> error: {e:#}"),
            }
        }
    }
}
