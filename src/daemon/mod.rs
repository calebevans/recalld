pub mod protocol;
pub mod lifecycle;
pub mod client;
pub mod bridge_adapters;
pub mod server;

pub use protocol::{DaemonRequest, DaemonResponse, DaemonRpcError};
pub use client::DaemonClient;
pub use server::DaemonServer;
pub use lifecycle::{socket_path, pid_path, is_daemon_alive, cleanup_stale_socket};
