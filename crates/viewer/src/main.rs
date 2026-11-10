//! Remote access viewer: a dashboard of computers, with each remote desktop in its own window.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod dashboard;
mod framebuffer;
mod geometry;
#[cfg_attr(windows, allow(dead_code))]
mod keymap;
mod net;
#[cfg(windows)]
mod session;
#[cfg(not(windows))]
#[path = "session_egui.rs"]
mod session;
mod store;
mod transfer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "rdviewer", about = "Remote access viewer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

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
    /// List, send or get files without opening a window. For diagnostics.
    #[command(hide = true)]
    Files {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        fingerprint: Option<String>,
        /// Folder to list; empty for the starting places.
        #[arg(long)]
        list: Option<String>,
        /// Local files to send into --dir.
        #[arg(long)]
        put: Vec<PathBuf>,
        #[arg(long)]
        dir: Option<String>,
        /// Remote file to copy to --to.
        #[arg(long)]
        get: Option<String>,
        #[arg(long)]
        to: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    #[cfg(windows)]
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
        Some(Command::Files { addr, fingerprint, list, put, dir, get, to }) => {
            let job = net::FileJob { list, put, dir, get, to };
            net::files(&addr, fingerprint.as_deref(), job)
        }
    }
}
