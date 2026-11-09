//! Remote access viewer: a dashboard of computers, with each remote desktop in its own window.

#![cfg_attr(not(test), windows_subsystem = "windows")]

#[cfg(windows)]
mod dashboard;
#[cfg(windows)]
mod framebuffer;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod session;
#[cfg(windows)]
mod store;

#[cfg(windows)]
use anyhow::Result;
#[cfg(windows)]
use clap::{Parser, Subcommand};
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
#[derive(Parser)]
#[command(name = "rdviewer", about = "Remote access viewer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(windows)]
#[derive(Subcommand)]
enum Command {
    /// Open a remote desktop window. The dashboard does this for you.
    Session {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        name: String,
    },
    /// Print this viewer's admin key, for `rdagent install --admin-key`.
    Key,
    /// Connect without opening a window, print statistics and optionally save a bitmap. For diagnostics.
    Snapshot {
        #[arg(long)]
        addr: String,
        /// Expected fingerprint. Omit to trust whatever certificate the agent presents.
        #[arg(long)]
        fingerprint: Option<String>,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[cfg(windows)]
fn main() -> Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.command, None | Some(Command::Session { .. })) {
        // This is a GUI program, so command-line output needs the parent's console.
        let _ = unsafe { windows::Win32::System::Console::AttachConsole(u32::MAX) };
    }
    match cli.command {
        None => dashboard::run(),
        Some(Command::Session { addr, fingerprint, name }) => session::run(addr, fingerprint, name),
        Some(Command::Key) => {
            println!("{}", hex::encode(rd_common::auth::public_key(&store::load_or_create_key()?)));
            Ok(())
        }
        Some(Command::Snapshot { addr, fingerprint, seconds, out }) => {
            net::snapshot(&addr, fingerprint.as_deref(), Duration::from_secs(seconds), out.as_deref())
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("rdviewer runs on Windows only. On a Mac, build just the agent: cargo build --release -p rd-agent");
    std::process::exit(1);
}
