//! Library half of evbar: the Everything IPC protocol, highlight parsing, and an
//! in-process mock of the Everything IPC window used by tests and examples.

pub mod explorer;
pub mod highlight;
pub mod hotkey;
pub mod ipc;
pub mod mock;
pub mod scope;

/// Encodes a Rust string as a NUL-terminated UTF-16 buffer for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
