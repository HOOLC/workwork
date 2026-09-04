use std::sync::{Mutex, OnceLock};

use ulid::Ulid;

static LAST_ULID: OnceLock<Mutex<Ulid>> = OnceLock::new();

fn last_ulid() -> &'static Mutex<Ulid> {
    LAST_ULID.get_or_init(|| Mutex::new(Ulid::nil()))
}

/// Locally minted identity. Caller-supplied keys stay caller-supplied.
pub(crate) fn new_ulid() -> String {
    let mut last = last_ulid().lock().expect("ULID generator mutex poisoned");
    let generated = Ulid::new();
    let next = if generated > *last {
        generated
    } else {
        Ulid(last.0.checked_add(1).expect("ULID space exhausted"))
    };
    *last = next;
    next.to_string()
}
