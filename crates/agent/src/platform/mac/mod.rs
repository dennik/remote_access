//! macOS backend. The agent runs as a launchd agent in the logged-in user's
//! session, which is where screen capture and input injection are allowed.
//! It needs the Screen Recording and Accessibility permissions.

pub mod capture;
pub mod files;
pub mod input;
pub mod install;
mod permissions;

use crate::config::Store;
use anyhow::Result;
use std::net::SocketAddr;

/// macOS has no separate secure desktop to follow, so this does nothing.
#[derive(Default)]
pub struct DesktopTracker;

impl DesktopTracker {
    pub fn sync(&mut self) -> bool {
        false
    }
}

/// macOS reports display geometry in points and captures in pixels, so there's nothing to set.
pub fn set_dpi_aware() {}

/// The launchd entry point.
pub fn run() -> Result<()> {
    let store = Store::mac_user()?;
    // Puts the agent in both permission lists, and prompts the first time.
    permissions::request(&store);
    let runtime = tokio::runtime::Runtime::new()?;
    let addr: SocketAddr = ([0, 0, 0, 0], rd_common::TCP_PORT).into();
    let discovery = Some((rd_common::DISCOVERY_PORT, rd_common::TCP_PORT));
    runtime.block_on(crate::server::serve(store, addr, discovery, |stream, store| async move {
        crate::session::run(stream, std::sync::Arc::new(move |line: &str| store.log(line))).await
    }))
}
