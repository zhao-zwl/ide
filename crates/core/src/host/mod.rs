//! Host abstraction layer (T01 — host-decoupling of the decision Core).
//!
//! The decision Core never imports a concrete IDE. It talks to the
//! [`HostProvider`] trait, which is implemented by:
//!   * [`CliHost`]        — an in-process CLI stub (proves decoupling),
//!   * [`GrpcHostClient`] — a remote host reached over the ProtoBus,
//!   * (future) the Tauri host.
//!
//! [`HostBridge`] is the single facade the Core uses; swapping the concrete
//! provider requires zero changes inside the Planner/LLM/etc.

pub mod bridge;
pub mod cli_host;
pub mod grpc_host_client;
pub mod provider;

pub use bridge::HostBridge;
pub use cli_host::CliHost;
pub use grpc_host_client::GrpcHostClient;
pub use provider::{GhostText, HostError, HostEvent, HostProvider, TextEdit};
