//! Fixed glue: the JSON-lines protocol (used by the `{{BIN}}` binary) and a C ABI (for hosts that
//! can load a shared library). Do not edit; implement `crate::port::call` instead.
//!
//! One request line `{"args":[...]}` gives one response line:
//!   `{"ok": VALUE}`      the port returned VALUE
//!   `{"err": "message"}` the port returned an error (where the original raises)
//!   `{"fatal": "why"}`   protocol problem or panic; the host should fall back to the original

use serde_json::{json, Value};
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

pub fn handle_line(line: &str) -> String {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"fatal": format!("bad request: {e}")}).to_string(),
    };
    let Some(args) = req.get("args").and_then(Value::as_array) else {
        return json!({"fatal": "request has no `args` array"}).to_string();
    };
    match catch_unwind(AssertUnwindSafe(|| crate::port::call(args))) {
        Ok(Ok(v)) => json!({"ok": v}).to_string(),
        Ok(Err(e)) => json!({"err": e}).to_string(),
        Err(_) => json!({"fatal": "panic in port"}).to_string(),
    }
}

/// C ABI: `request` is a NUL-terminated UTF-8 request line; the returned NUL-terminated response
/// line must be released with `rcargo_port_free`.
///
/// # Safety
/// `request` must be null or point to a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn rcargo_port_call(request: *const c_char) -> *mut c_char {
    let line = if request.is_null() {
        String::new()
    } else {
        CStr::from_ptr(request).to_string_lossy().into_owned()
    };
    CString::new(handle_line(&line)).unwrap_or_default().into_raw()
}

/// # Safety
/// `p` must come from `rcargo_port_call` and not be freed twice.
#[no_mangle]
pub unsafe extern "C" fn rcargo_port_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}
