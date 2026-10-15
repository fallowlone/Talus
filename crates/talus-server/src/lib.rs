//! Self-hosted sync server for Talus shared workspaces: accounts, roles and connection
//! definitions (never secrets). It speaks plain HTTP; run it behind a TLS-terminating reverse proxy.

mod api;
mod store;

pub use api::{router, serve};
pub use store::{ApiError, MIN_PASSWORD_LEN, Role, Store};
