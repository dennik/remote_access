//! Remote access agent, installed on each supervised computer.
//!
//! Windows: `service` runs as a SYSTEM service from boot. It owns the network
//! listener and logins, and starts a `helper` in the console session for each
//! connection, because a service in Session 0 can't capture or inject input.
//!
//! macOS: `run` is started by launchd in the logged-in user's session and does
//! everything in one process. The login window and lock screen are out of reach.
//!
//! `console` runs in the foreground for testing on either platform.

mod capture;
mod config;
mod diff;
mod keymap;
mod platform;
mod server;
mod session;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rdagent", about = "Remote access agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install or update the agent. Needs an elevated prompt on Windows, or sudo on macOS.
    Install {
        /// The admin key shown in the viewer's dashboard. Repeat to authorize several viewers.
        #[arg(long = "admin-key", required = true)]
        admin_keys: Vec<String>,
    },
    /// Remove the agent, firewall rules, keys and logs. Needs an elevated prompt on Windows, or sudo on macOS.
    Uninstall,
    /// Print this machine's name and certificate fingerprint.
    Info,
    /// Run in the foreground in the current session, for testing.
    Console {
        #[arg(long, default_value_t = rd_common::TCP_PORT)]
        port: u16,
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Directory for the test certificate and authorized keys.
        #[arg(long)]
        data_dir: std::path::PathBuf,
        /// Admin key to authorize, in addition to any already in the directory.
        #[arg(long = "admin-key")]
        admin_keys: Vec<String>,
    },
    /// Entry point for the Windows service manager.
    #[cfg(windows)]
    #[command(hide = true)]
    Service,
    /// Started by the service in the console session.
    #[cfg(windows)]
    #[command(hide = true)]
    Helper {
        #[arg(long)]
        pipe: String,
    },
    /// Entry point for launchd, in the logged-in user's session.
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    Run,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install { admin_keys } => platform::install::install(&admin_keys),
        Command::Uninstall => platform::install::uninstall(),
        Command::Info => platform::install::info(),
        Command::Console { port, bind, data_dir, admin_keys } => {
            let store = config::Store::new(data_dir);
            store.add_authorized_keys(&admin_keys)?;
            println!("fingerprint {}", store.identity()?.fingerprint);
            server::run_console(store, &bind, port)
        }
        #[cfg(windows)]
        Command::Service => platform::service::run(),
        #[cfg(windows)]
        Command::Helper { pipe } => platform::service::run_helper(&pipe),
        #[cfg(target_os = "macos")]
        Command::Run => platform::run(),
    }
}
