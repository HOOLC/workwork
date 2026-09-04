//! 会话运行时。目标架构见 `docs/zork-agent-architecture.md`。

pub mod compression;
pub mod context;
pub mod deadline;
pub mod decision;
pub mod event_id;
pub mod events;
pub mod executor;
pub mod log;
pub mod model;
pub mod ports;
pub mod projection;
pub mod query;
pub mod recovery;
pub mod runner;
pub mod service;
pub mod state;
pub mod store;
pub mod supervisor;
pub mod tools;
pub mod wire;

pub use state::SessionState;
pub use store::StreamStore;
