use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ulid::Ulid;
use zork_agent::session::ports::IdGenerator;

#[derive(Clone, Debug)]
pub struct DeterministicIds {
    next: Arc<AtomicU64>,
    prefix: u128,
}

impl DeterministicIds {
    pub fn new(first: u64) -> Self {
        Self {
            next: Arc::new(AtomicU64::new(first)),
            prefix: 1_u128 << 80,
        }
    }
}

impl Default for DeterministicIds {
    fn default() -> Self {
        Self::new(1)
    }
}

impl IdGenerator for DeterministicIds {
    fn next(&self) -> String {
        let value = self.next.fetch_add(1, Ordering::Relaxed);
        Ulid::from(self.prefix | u128::from(value)).to_string()
    }
}
