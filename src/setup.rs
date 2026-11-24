//! `rumpel setup`: tokens, a starting config.toml, the agents' home repo and a service
//! that keeps `rumpel run` going. Safe to rerun: Enter keeps what is already there.

use crate::slack::{Res, config_dir};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub async fn setup() -> Res<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    chmod(&dir, 0o700)?;
    println!("Writing to {}\n", dir.display());

    token(&dir.join("bot-token"), "Bot token (OAuth & Permissions, xoxb-…)", "xoxb-")?;
    token(&dir.join("app-token"), "App-level token (Basic Information, scope connections:write, xapp-…)", "xapp-")?;

    let config = dir.join("config.toml");
    if config.exists() {
        println!("kept    {}", config.display());
    } else {
        let owner = ask("Your Slack user id (profile -> ⋮ -> Copy member ID)", None)?;
        if !owner.starts_with(['U', 'W']) {
            return Err(format!("{owner} is not a user id (U… or W…)"));
        }
        let home = expand_home(&ask("Home repo for the bot's agents", Some("~/rumpel-home"))?);
        if !home.join("AGENTS.md").exists() {
            let name = ask("No AGENTS.md there. Create the repo with `rumpel init`; the bot's name", Some("Rumpel"))?;
            crate::init(&home, &name)?;
        }
        write_private(&config, &render_config(&owner, &home))?;
        println!("created {} (DMs and @mentions from you, in {})", config.display(), home.display());
    }

    println!();
    crate::check(config.clone()).await?;

    if ask("\nInstall and start the rumpel service? Run only one per Slack app. [Y/n]", Some("y"))?.to_lowercase().starts_with('n') {
        println!("Skipped. Start it yourself with `rumpel run`.");
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = std::env::var("PATH").unwrap_or_default();
    if cfg!(target_os = "macos") { launchd(&exe, &path) } else { systemd(&exe, &path) }
}

/// The starting config: the owner's DMs and @mentions in any channel go to agents in `home`.
fn render_config(owner: &str, home: &Path) -> String {
    let repo = home.display();
    format!(
        r#"# Written by `rumpel setup`; every key is explained in the README.
owner = "{owner}"

[paseo]
provider = "claude/sonnet"
mode = "bypassPermissions"

# Every channel the bot is invited to.
[[channel]]
id = "*"
listen = "mention"
from = "owner"
repo = "{repo}"
workspace = "local"
hints = false
session = "thread"

# Direct messages: one ongoing agent per DM partner.
[[channel]]
id = "dm"
listen = "all"
from = "owner"
repo = "{repo}"
workspace = "local"
hints = false
session = "channel"
loading = false
"#
    )
}

fn systemd_unit(exe: &Path, path: &str) -> String {
    format!(
        "[Unit]\nDescription=rumpel (Slack -> Paseo agents)\nAfter=network-online.target\n\n\
         [Service]\nEnvironment=PATH={path}\nExecStart={} run\nRestart=always\nRestartSec=10\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display()
    )
}

fn launchd_plist(exe: &Path, path: &str, log: &Path) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (exe, log) = (esc(&exe.display().to_string()), esc(&log.display().to_string()));
    let path = esc(path);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>rumpel</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>run</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

fn systemd(exe: &Path, path: &str) -> Res<()> {
    let unit = home().join(".config/systemd/user/rumpel.service");
    write_file(&unit, &systemd_unit(exe, path))?;
    sh("systemctl", &["--user", "daemon-reload"])?;
    sh("systemctl", &["--user", "enable", "rumpel"])?;
    sh("systemctl", &["--user", "restart", "rumpel"])?;
    println!(
        "Service started ({}). Logs: journalctl --user -u rumpel -f\n\
         To keep it running while you are logged out: sudo loginctl enable-linger $USER",
        unit.display()
    );
    Ok(())
}

fn launchd(exe: &Path, path: &str) -> Res<()> {
    let plist = home().join("Library/LaunchAgents/rumpel.plist");
    let log = home().join("Library/Logs/rumpel.log");
    write_file(&plist, &launchd_plist(exe, path, &log))?;
    let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output().map_err(|e| e.to_string())?.stdout).trim().to_string();
    // Replace a running copy; bootout fails harmlessly when none is loaded.
    let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/rumpel")]).output();
    sh("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist.display().to_string()])?;
    println!("Service started ({}). Logs: tail -f {}", plist.display(), log.display());
    Ok(())
}

/// Asks for a token without echoing it; Enter keeps the saved one.
fn token(file: &Path, label: &str, prefix: &str) -> Res<()> {
    let saved = file.exists();
    let hint = if saved { " [Enter keeps the saved one]" } else { "" };
    print!("{label}{hint}: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let _ = Command::new("stty").arg("-echo").stderr(std::process::Stdio::null()).status();
    let line = read_line();
    let _ = Command::new("stty").arg("echo").stderr(std::process::Stdio::null()).status();
    println!();
    let t = line?;
    if t.is_empty() && saved {
        return Ok(());
    }
    if !t.starts_with(prefix) {
        return Err(format!("expected a token starting with {prefix}"));
    }
    write_private(file, &t)
}

fn ask(question: &str, default: Option<&str>) -> Res<String> {
    match default {
        Some(d) if !question.ends_with(']') => print!("{question} [{d}]: "),
        _ => print!("{question}: "),
    }
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let answer = read_line()?;
    match (answer.is_empty(), default) {
        (true, Some(d)) => Ok(d.into()),
        (true, None) => Err("no answer".into()),
        (false, _) => Ok(answer),
    }
}

fn read_line() -> Res<String> {
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s).map_err(|e| e.to_string())?;
    Ok(s.trim().to_string())
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn write_file(path: &Path, content: &str) -> Res<()> {
    std::fs::create_dir_all(path.parent().expect("has parent")).map_err(|e| e.to_string())?;
    std::fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))
}

/// Mode 600 before the content lands, so a token is never readable by others.
fn write_private(path: &Path, content: &str) -> Res<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    chmod(path, 0o600)?;
    f.write_all(content.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn chmod(path: &Path, mode: u32) -> Res<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| format!("{}: {e}", path.display()))
}

fn sh(cmd: &str, args: &[&str]) -> Res<()> {
    let ok = Command::new(cmd).args(args).status().map_err(|e| format!("{cmd}: {e}"))?;
    if ok.success() { Ok(()) } else { Err(format!("{cmd} {} failed", args.join(" "))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Me, Message};

    #[test]
    fn rendered_config_answers_the_owner() {
        let cfg: Config = toml::from_str(&render_config("UOWNER", Path::new("/h/bot"))).unwrap();
        let me = Me { user: "UBOT".into(), bot: "BBOT".into() };
        let msg = |channel: &str, user: &str, text: &str, kind: &str| Message {
            channel: channel.into(),
            user: Some(user.into()),
            text: text.into(),
            ts: "1.0".into(),
            thread_ts: None,
            subtype: None,
            bot_id: None,
            channel_type: Some(kind.into()),
        };
        let dm = cfg.should_trigger(&me, &msg("D1", "UOWNER", "hi", "im")).expect("owner DM");
        assert_eq!(dm.repo.as_deref(), Some(Path::new("/h/bot")));
        assert!(cfg.should_trigger(&me, &msg("C1", "UOWNER", "<@UBOT> hi", "channel")).is_some(), "mention");
        assert!(cfg.should_trigger(&me, &msg("C1", "UOWNER", "hi", "channel")).is_none(), "no mention");
        assert!(cfg.should_trigger(&me, &msg("D2", "UX", "hi", "im")).is_none(), "someone else");
    }

    #[test]
    fn services_run_this_binary() {
        let exe = Path::new("/opt/bin/rumpel");
        assert!(systemd_unit(exe, "/a:/b").contains("\nExecStart=/opt/bin/rumpel run\n"));
        assert!(systemd_unit(exe, "/a:/b").contains("\nEnvironment=PATH=/a:/b\n"));
        let plist = launchd_plist(exe, "/a&b", Path::new("/l.log"));
        assert!(plist.contains("<string>/opt/bin/rumpel</string><string>run</string>"));
        assert!(plist.contains("<string>/a&amp;b</string>"));
    }

    #[test]
    fn tilde_expands_to_home() {
        assert_eq!(expand_home("~/x"), home().join("x"));
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
