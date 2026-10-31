pub mod agent;
pub mod context;
pub mod crypto;
pub mod devices;
#[cfg(unix)]
pub mod host;
#[cfg(unix)]
pub mod ipc;
pub mod memory;
pub mod model;
pub mod permissions;
pub mod protocol;
#[cfg(unix)]
pub mod relay;
pub mod relay_protocol;
pub mod scheduler;
pub mod scheduling;
pub mod selector;
pub mod service;
#[cfg(unix)]
pub mod skills;
pub mod storage;
pub mod tools;
pub mod transport;
#[cfg(unix)]
pub mod tui;
#[cfg(unix)]
pub mod update;
#[cfg(unix)]
pub mod vault;
#[cfg(unix)]
pub mod worker;

pub mod credentials;

pub const VERSION: &str = match option_env!("VORRA_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
