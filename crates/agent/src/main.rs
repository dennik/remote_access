//! Remote access agent, installed on each supervised computer.
//!
//! `service` runs as a SYSTEM service from boot: it owns the network listener and
//! logins, and starts a `helper` in the console session for each connection.
//! The helper does the capture and input injection, which a service in Session 0
//! cannot do. `console` runs both roles in one process for testing, without
//! admin rights, but it can't see the lock screen or UAC prompts.

mod capture;
mod config;
mod desktop;
mod diff;
mod input;
mod install;
mod server;
mod service;
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
    /// Install or update the service. Needs an elevated prompt.
    Install {
        /// The admin key shown in the viewer's dashboard. Repeat to authorize several viewers.
        #[arg(long = "admin-key", required = true)]
        admin_keys: Vec<String>,
    },
    /// Remove the service, firewall rules, keys and logs. Needs an elevated prompt.
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
    #[command(hide = true)]
    Service,
    /// Started by the service in the console session.
    #[command(hide = true)]
    Helper {
        #[arg(long)]
        pipe: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Install { admin_keys } => install::install(&admin_keys),
        Command::Uninstall => install::uninstall(),
        Command::Info => install::info(),
        Command::Console { port, bind, data_dir, admin_keys } => {
            let store = config::Store::new(data_dir);
            store.add_authorized_keys(&admin_keys)?;
            println!("fingerprint {}", store.identity()?.fingerprint);
            server::run_console(store, &bind, port)
        }
        Command::Service => service::run(),
        Command::Helper { pipe } => service::run_helper(&pipe),
    }
}
