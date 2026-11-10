pub mod capture;
mod desktop;
pub mod files;
pub mod input;
pub mod install;
pub mod service;

pub use desktop::DesktopTracker;
pub use service::set_dpi_aware;
