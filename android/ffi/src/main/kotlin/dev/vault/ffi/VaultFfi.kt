// SPDX-License-Identifier: MIT
package dev.vault.ffi

/**
 * The vault-ffi bindings are generated at build time into the package
 * `uniffi.vault_ffi` (see this module's build file); JNA loads
 * `libvault_ffi.so` from the APK on first use.
 */
object VaultFfi {
    /** The native library's name, as JNA and the generated bindings load it. */
    const val LIBRARY: String = "vault_ffi"
}
