// SPDX-License-Identifier: MIT
package dev.vault.core

/**
 * One vault on this phone as the Vaults list shows it. A vault is identified by
 * the exact normalized `(appId, origin)` pair; [id] names its directory.
 */
data class VaultSummary(
    val id: String,
    val appId: String,
    val origin: String,
)
