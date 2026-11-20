// SPDX-License-Identifier: MIT
//! The UniFFI bindings generator, pinned to this crate's UniFFI version.
//!
//! ```text
//! cargo build -p vault-ffi --release
//! cargo run -p vault-ffi --features bindgen --bin uniffi-bindgen -- \
//!     generate --library target/release/libvault_ffi.so --language kotlin --out-dir OUT
//! ```

fn main() {
    uniffi::uniffi_bindgen_main()
}
