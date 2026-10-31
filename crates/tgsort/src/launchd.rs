use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "com.tgsort.agent";

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("cannot determine home directory")
}

pub fn plist_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn log_path() -> Result<PathBuf> {
    Ok(home()?.join("Library/Logs/tgsort.log"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plist(exe: &Path, config: Option<&Path>, log: &Path) -> String {
    let mut args = vec![exe.display().to_string()];
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    args.push("run".into());
    let args: String = args.iter().map(|a| format!("    <string>{}</string>\n", xml(a))).collect();
    let log = xml(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output().context("running `id -u`")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn launchctl(args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new("launchctl").args(args).output().context("running launchctl")?;
    let msg = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), msg.trim().to_string()))
}

pub fn install(config_arg: Option<&Path>, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        bail!("config {} not found; run `tgsort init` first", config_path.display());
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    let config = config_arg.map(std::path::absolute).transpose()?;
    let (plist_file, log) = (plist_path()?, log_path()?);
    for p in [&plist_file, &log] {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
    }
    std::fs::write(&plist_file, plist(&exe, config.as_deref(), &log))?;
    println!("wrote {}", plist_file.display());

    let target = format!("gui/{}", uid()?);
    let service = format!("{target}/{LABEL}");
    if launchctl(&["print", &service])?.0 {
        println!("agent already loaded; reloading it");
        launchctl(&["bootout", &service])?;
    }
    let (ok, msg) = launchctl(&["bootstrap", &target, &plist_file.display().to_string()])?;
    if !ok {
        bail!("launchctl bootstrap failed: {msg}");
    }
    println!("agent loaded; logs: {}", log.display());
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let service = format!("gui/{}/{LABEL}", uid()?);
    let (ok, msg) = launchctl(&["bootout", &service])?;
    if ok {
        println!("agent unloaded");
    } else {
        println!("agent was not loaded ({msg})");
    }
    let plist_file = plist_path()?;
    if plist_file.exists() {
        std::fs::remove_file(&plist_file)?;
        println!("removed {}", plist_file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_args_and_escapes() {
        let p = plist(Path::new("/bin/tgsort"), Some(Path::new("/a&b/c.toml")), Path::new("/l.log"));
        assert!(p.contains("<string>/bin/tgsort</string>\n    <string>--config</string>\n    <string>/a&amp;b/c.toml</string>\n    <string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        let p = plist(Path::new("/bin/tgsort"), None, Path::new("/l.log"));
        assert!(!p.contains("--config"));
    }
}
