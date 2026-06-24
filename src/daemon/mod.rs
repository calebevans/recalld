pub mod bridge_adapters;
pub mod client;
pub mod lifecycle;
pub mod protocol;
pub mod server;

pub use client::DaemonClient;
pub use lifecycle::{cleanup_stale_socket, is_daemon_alive, pid_path, socket_path};
pub use protocol::{DaemonRequest, DaemonResponse, DaemonRpcError};
pub use server::DaemonServer;
