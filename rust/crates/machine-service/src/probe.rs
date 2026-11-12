//! The host probe: `machine-service --probe-host` (`docs/arch/machines.md`,
//! MM-3).
//!
//! The service runs its own executable with this flag as a child, so a
//! crash or a hang in Metal never reaches the daemon: the parent bounds the
//! child with a timeout. It prints one JSON object with the `gpu` section
//! and changes nothing. It checks no privacy permission: the service knows
//! nothing about them (DC-309).

use std::ffi::{c_char, c_void, CStr};

use serde_json::{json, Value};

#[link(name = "objc")]
extern "C" {
    fn objc_msgSend();
    fn sel_registerName(name: *const c_char) -> *mut c_void;
}

#[link(name = "Metal", kind = "framework")]
extern "C" {
    fn MTLCreateSystemDefaultDevice() -> *mut c_void;
}

/// `MTLGPUFamily` values and their report names.
const FAMILIES: [(&str, i64); 17] = [
    ("apple1", 1001),
    ("apple2", 1002),
    ("apple3", 1003),
    ("apple4", 1004),
    ("apple5", 1005),
    ("apple6", 1006),
    ("apple7", 1007),
    ("apple8", 1008),
    ("apple9", 1009),
    ("apple10", 1010),
    ("mac2", 2002),
    ("common1", 3001),
    ("common2", 3002),
    ("common3", 3003),
    ("metal3", 5001),
    ("metal4", 5002),
    ("mac_catalyst2", 4002),
];

fn selector(name: &CStr) -> *mut c_void {
    // SAFETY: `name` is a NUL-terminated selector name.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// The GPU section, or `None` when there is no Metal device.
fn gpu() -> Option<Value> {
    // SAFETY: `MTLCreateSystemDefaultDevice` takes no arguments and returns
    // a retained device or null. The process exits right after the probe,
    // so the device is never released.
    let device = unsafe { MTLCreateSystemDefaultDevice() };
    if device.is_null() {
        return None;
    }
    // SAFETY: each cast gives `objc_msgSend` the exact prototype of the
    // method it sends: `-supportsFamily:(MTLGPUFamily)` returns BOOL,
    // `-name` returns an object, `-UTF8String` a C string, and
    // `-argumentBuffersSupport` an NSUInteger.
    unsafe {
        let send = objc_msgSend as unsafe extern "C" fn();
        let supports: extern "C" fn(*mut c_void, *mut c_void, i64) -> bool =
            std::mem::transmute(send);
        let object: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(send);
        let unsigned: extern "C" fn(*mut c_void, *mut c_void) -> u64 = std::mem::transmute(send);

        let supports_family = selector(c"supportsFamily:");
        let families: Vec<&str> = FAMILIES
            .iter()
            .filter(|(_, value)| supports(device, supports_family, *value))
            .map(|(name, _)| *name)
            .collect();
        let name_object = object(device, selector(c"name"));
        let name = if name_object.is_null() {
            None
        } else {
            let text = object(name_object, selector(c"UTF8String")) as *const c_char;
            (!text.is_null()).then(|| CStr::from_ptr(text).to_string_lossy().to_string())
        };
        // MTLArgumentBuffersTier1 is 0 and Tier2 is 1.
        let tier = unsigned(device, selector(c"argumentBuffersSupport")) + 1;
        Some(json!({
            "name": name,
            "metal_families": families,
            "metal3": families.contains(&"metal3"),
            "argument_buffers_tier": tier,
        }))
    }
}

/// Print the probe's answer.
pub fn run() {
    let answer = json!({"gpu": gpu()});
    println!("{answer}");
}
