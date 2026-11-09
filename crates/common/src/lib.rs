//! Wire protocol, authentication and TLS shared by the agent and the viewer.

pub mod auth;
pub mod clipboard;
pub mod proto;
pub mod tls;

pub const TCP_PORT: u16 = 47800;
pub const DISCOVERY_PORT: u16 = 47801;
pub const DISCOVERY_REQUEST: &[u8] = b"RDDISCOVER1";
