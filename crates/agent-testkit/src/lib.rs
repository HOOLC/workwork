//! Deterministic, directly programmable test environments for zork-agent.
//!
//! Each effect controller is a separate module. [`TestWorld`] composes the
//! in-memory implementations; [`RealAgent`] keeps production storage, tools,
//! clock and process execution while replacing only provider and mailbox input.

pub mod clock;
pub mod codex_provider;
pub mod filesystem;
pub mod identity;
pub mod model;
pub mod performance;
pub mod process;
pub mod provider;
pub mod query;
pub mod query_performance;
pub mod query_pressure;
pub mod real_agent;
mod server;
pub mod startup_performance;
pub mod store;
pub mod tool;
pub mod world;

pub use clock::{ManualClock, PausedTimeIoGuard};
pub use codex_provider::{ControlledCodexProvider, PendingCodexRequest};
pub use filesystem::MemoryFileSystem;
pub use identity::DeterministicIds;
pub use model::{ControlledModel, PendingModelRequest};
pub use performance::{measure_virtual_sessions, VirtualPerformance};
pub use process::{ControlledProcesses, PendingProcess};
pub use provider::{
    ControlledHttpProvider, PendingHttpRequest, ProviderBodyStream, ProviderSseStream,
};
pub use query::MemorySessionQuery;
pub use query_performance::{
    measure_history_fragment_queries, measure_query_apis, HistoryFragmentPerformance,
    QueryApiPerformance, QueryPerformanceError,
};
pub use query_pressure::{
    measure_query_pressure, prepare_query_pressure_fixture, QueryPressureManifest,
    QueryPressureMode, QueryPressurePerformance,
};
pub use real_agent::{RealAgent, RealAgentError};
pub use server::AgentHttpServer;
pub use startup_performance::{
    measure_real_startup, prepare_real_startup_fixture, RealStartupLimits, RealStartupPerformance,
    StartupPerformanceError,
};
pub use store::MemorySessionStore;
pub use tool::{ControlledTool, PendingToolRequest};
pub use world::{TestWorld, TestWorldError};
