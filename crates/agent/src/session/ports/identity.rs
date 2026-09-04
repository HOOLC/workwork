/// Generates zork-agent-owned opaque identities. Event IDs remain owned by the
/// session store because they are session-local ordered cursors.
pub trait IdGenerator: Send + Sync {
    fn next(&self) -> String;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemIdGenerator;

impl IdGenerator for SystemIdGenerator {
    fn next(&self) -> String {
        crate::ids::new_ulid()
    }
}
