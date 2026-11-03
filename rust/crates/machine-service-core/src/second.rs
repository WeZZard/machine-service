//! Whether this daemon is a second instance (`second_instance::from_env`,
//! DC-1xx-LIVE-2), set once at startup by the daemon's `main` after it has
//! checked its environment. A process that never sets it, such as a
//! component test's service, is not a second instance.

use std::sync::OnceLock;

pub use second_instance::Marker;

static MARKER: OnceLock<Marker> = OnceLock::new();

/// Record that this process is the second instance `marker`; the first
/// call wins.
pub fn set(marker: Marker) {
    let _ = MARKER.set(marker);
}

/// This process's mark, when it is a second instance.
pub fn get() -> Option<&'static Marker> {
    MARKER.get()
}

/// The prefix every VM this process clones carries: the second instance's
/// own, else none.
pub fn vm_prefix() -> String {
    get().map(Marker::vm_prefix).unwrap_or_default()
}
