//! Independently replaceable external-effect boundaries.

mod clock;
mod filesystem;
mod identity;
mod model;
mod process;
mod profile;

pub use clock::{Clock, Sleep, SystemClock};
pub use filesystem::{FilePage, FileSystem, SystemFileSystem};
pub use identity::{IdGenerator, SystemIdGenerator};
pub use model::{ModelExecutor, ModelPort};
pub use process::{
    ProcessHandle, ProcessRequest, ProcessSpawner, ProcessStatus, SpawnedProcess,
    SystemProcessSpawner,
};
pub use profile::{ModelLimits, ProfileExecution, ProfileResolveError, ProfileResolver};
