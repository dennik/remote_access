//! Installs the agent as a launchd agent that starts when the user logs in.
//! Run with sudo while the user who should be visible is logged in.

use crate::config::{self, Store};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const LABEL: &str = "com.remoteaccess.agent";
const PLIST: &str = "/Library/LaunchAgents/com.remoteaccess.agent.plist";
const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

fn binary() -> PathBuf {
    config::mac_shared_dir().join("rdagent")
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn must(program: &str, args: &[&str]) -> Result<()> {
    output(program, args).map(|_| ())
}

fn require_root() -> Result<()> {
    if output("id", &["-u"])? != "0" {
        bail!("run this with sudo, for example: sudo ./rdagent install --admin-key <key>");
    }
    Ok(())
}

/// The user logged in at the screen, whose session the agent runs in.
fn console_user() -> Result<(String, String, PathBuf)> {
    let user = output("stat", &["-f", "%Su", "/dev/console"])?;
    if user.is_empty() || user == "root" {
        bail!("nobody is logged in at the screen. Log in as the user the Mac normally runs as, then install again.");
    }
    let uid = output("id", &["-u", &user])?;
    let home = output("dscl", &[".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"])?;
    let home = home.split_once(':').map(|(_, h)| h.trim().to_string()).unwrap_or_default();
    if home.is_empty() {
        bail!("couldn't find the home folder of {user}");
    }
    Ok((user, uid, PathBuf::from(home)))
}

fn plist(binary: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>run</string>
    </array>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
</dict>
</plist>
"#,
        binary.display()
    )
}

pub fn install(admin_keys: &[String]) -> Result<()> {
    for key in admin_keys {
        config::parse_key(key.trim())?;
    }
    require_root()?;
    let (user, uid, home) = console_user()?;
    let domain = format!("gui/{uid}");
    let _ = output("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);

    // The program and authorized keys are root-owned, so the user can't change who gets in.
    let shared = config::mac_shared_dir();
    std::fs::create_dir_all(&shared)?;
    let target = binary();
    let source = std::env::current_exe()?;
    if source != target {
        std::fs::copy(&source, &target).with_context(|| format!("copying to {}", target.display()))?;
    }
    let shared_str = shared.to_string_lossy().to_string();
    must("chown", &["-R", "root:wheel", &shared_str])?;
    must("chmod", &["755", &shared_str, &target.to_string_lossy()])?;
    Store::with_dirs(shared.clone(), shared.clone()).add_authorized_keys(admin_keys)?;
    must("chmod", &["644", &shared.join("authorized_keys").to_string_lossy()])?;

    // The identity belongs to the user the agent runs as, and only they can read it.
    let user_dir = config::mac_user_dir(&home);
    let identity = Store::new(user_dir.clone()).identity()?;
    let user_dir_str = user_dir.to_string_lossy().to_string();
    must("chown", &["-R", &format!("{user}:staff"), &user_dir_str])?;
    must("chmod", &["-R", "go-rwx", &user_dir_str])?;

    std::fs::write(PLIST, plist(&target))?;
    must("chown", &["root:wheel", PLIST])?;
    must("chmod", &["644", PLIST])?;

    // Without this, the application firewall may block incoming connections.
    let target_str = target.to_string_lossy().to_string();
    let _ = output(FIREWALL, &["--add", &target_str]);
    let _ = output(FIREWALL, &["--unblockapp", &target_str]);

    // launchd can still be unloading the previous copy, so retry briefly.
    let mut started = output("launchctl", &["bootstrap", &domain, PLIST]);
    for _ in 0..5 {
        if started.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        started = output("launchctl", &["bootstrap", &domain, PLIST]);
    }
    started?;

    println!("Remote Access Agent is installed and running for {user}.");
    println!("Computer:    {}", config::hostname());
    println!("Fingerprint: {}", identity.fingerprint);
    println!();
    println!("Two more steps on this Mac, in System Settings > Privacy & Security:");
    println!("  1. Screen Recording: turn on rdagent (without it, viewers see only the wallpaper).");
    println!("  2. Accessibility: turn on rdagent (without it, viewers can't control the Mac).");
    println!("If rdagent isn't listed, click + and choose {}.", target.display());
    println!("Then restart the agent with: sudo launchctl kickstart -k {domain}/{LABEL}");
    println!();
    println!("The agent runs while {user} is logged in. For unattended Macs, turn on automatic login");
    println!("in System Settings > Users & Groups, so it's reachable again after a restart.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_root()?;
    if let Ok((_, uid, home)) = console_user() {
        let _ = output("launchctl", &["bootout", &format!("gui/{uid}/{LABEL}")]);
        let _ = std::fs::remove_dir_all(config::mac_user_dir(&home));
    }
    let target = binary().to_string_lossy().to_string();
    let _ = output(FIREWALL, &["--remove", &target]);
    let _ = std::fs::remove_file(PLIST);
    let _ = std::fs::remove_dir_all(config::mac_shared_dir());
    println!("Remote Access Agent has been removed.");
    println!("You can also remove rdagent from Screen Recording and Accessibility in System Settings.");
    Ok(())
}

pub fn info() -> Result<()> {
    println!("Computer:    {}", config::hostname());
    // The identity lives in the logged-in user's folder, so read it as that user.
    let home = match console_user() {
        Ok((_, _, home)) => home,
        Err(_) => PathBuf::from(std::env::var("HOME").unwrap_or_default()),
    };
    let dir = config::mac_user_dir(&home);
    match std::fs::read(dir.join("agent.crt")) {
        Ok(cert) => println!("Fingerprint: {}", rd_common::tls::fingerprint(&cert)),
        Err(e) => println!("Fingerprint: unavailable ({e}). Run this with sudo, or install first."),
    }
    Ok(())
}
