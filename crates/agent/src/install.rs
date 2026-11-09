//! Installs and removes the agent on this machine.

use crate::config::{self, Store};
use crate::service::SERVICE_NAME;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const FIREWALL_TCP: &str = "Remote Access Agent (TCP)";
const FIREWALL_UDP: &str = "Remote Access Agent (discovery)";
const POLICY_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

fn install_dir() -> PathBuf {
    let root = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(root).join("RemoteAccess")
}

fn run(program: &str, args: &[&str]) -> Result<bool> {
    let output = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    Ok(output.status.success())
}

fn must(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn require_admin() -> Result<()> {
    // `net session` succeeds only from an elevated prompt.
    if !run("net", &["session"])? {
        bail!("this command needs an elevated prompt: right-click Terminal and choose Run as administrator");
    }
    Ok(())
}

fn stop_service() -> Result<()> {
    if run("sc", &["query", SERVICE_NAME])? {
        let _ = run("sc", &["stop", SERVICE_NAME]);
        // The executable stays locked until the process has exited.
        for _ in 0..40 {
            let output = Command::new("sc").args(["query", SERVICE_NAME]).output()?;
            if String::from_utf8_lossy(&output.stdout).contains("STOPPED") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Ok(())
}

pub fn install(admin_keys: &[String]) -> Result<()> {
    for key in admin_keys {
        config::parse_key(key.trim())?;
    }
    require_admin()?;

    stop_service()?;

    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let target = dir.join("rdagent.exe");
    let source = std::env::current_exe()?;
    if source != target {
        std::fs::copy(&source, &target).with_context(|| format!("copying to {}", target.display()))?;
    }

    // State is private to SYSTEM and Administrators, set before any key is written.
    let store = Store::system();
    let data = store.dir().clone();
    std::fs::create_dir_all(&data)?;
    let data_str = data.to_string_lossy().to_string();
    must(
        "icacls",
        &[&data_str, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F"],
    )?;
    store.add_authorized_keys(admin_keys)?;
    let identity = store.identity()?;

    let target_str = format!("\"{}\" service", target.display());
    if run("sc", &["query", SERVICE_NAME])? {
        must("sc", &["config", SERVICE_NAME, "binPath=", &target_str, "start=", "auto"])?;
    } else {
        must(
            "sc",
            &["create", SERVICE_NAME, "binPath=", &target_str, "start=", "auto", "DisplayName=", "Remote Access Agent"],
        )?;
    }
    must("sc", &["description", SERVICE_NAME, "Lets an authorized administrator view and control this computer over the local network."])?;
    must("sc", &["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/5000/restart/5000/restart/30000"])?;

    // Reachable from the local subnet only.
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_TCP}")]);
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_UDP}")]);
    let tcp_port = format!("localport={}", rd_common::TCP_PORT);
    let udp_port = format!("localport={}", rd_common::DISCOVERY_PORT);
    must("netsh", &["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_TCP}"), "dir=in", "action=allow", "protocol=TCP", &tcp_port, "remoteip=localsubnet", "profile=any"])?;
    must("netsh", &["advfirewall", "firewall", "add", "rule", &format!("name={FIREWALL_UDP}"), "dir=in", "action=allow", "protocol=UDP", &udp_port, "remoteip=localsubnet", "profile=any"])?;

    // 1 lets services send Ctrl+Alt+Del.
    must("reg", &["add", POLICY_KEY, "/v", "SoftwareSASGeneration", "/t", "REG_DWORD", "/d", "1", "/f"])?;

    must("sc", &["start", SERVICE_NAME])?;

    println!("Remote Access Agent is installed and running.");
    println!("Computer:    {}", config::hostname());
    println!("Fingerprint: {}", identity.fingerprint);
    println!("Check this fingerprint when the viewer asks you to trust this computer.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_admin()?;
    stop_service()?;
    let _ = run("sc", &["delete", SERVICE_NAME]);
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_TCP}")]);
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", &format!("name={FIREWALL_UDP}")]);
    let _ = run("reg", &["delete", POLICY_KEY, "/v", "SoftwareSASGeneration", "/f"]);
    let _ = std::fs::remove_dir_all(Store::system().dir());

    let dir = install_dir();
    let running_from_install = std::env::current_exe().is_ok_and(|exe| exe.starts_with(&dir));
    if running_from_install {
        println!("Service removed. Delete {} after this window closes.", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        println!("Remote Access Agent has been removed.");
    }
    Ok(())
}

pub fn info() -> Result<()> {
    println!("Computer:    {}", config::hostname());
    match Store::system().identity() {
        Ok(identity) => println!("Fingerprint: {}", identity.fingerprint),
        Err(e) => println!("Fingerprint: unavailable ({e}). Run this from an elevated prompt."),
    }
    Ok(())
}
