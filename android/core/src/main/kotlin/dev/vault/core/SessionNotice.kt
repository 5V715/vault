// SPDX-License-Identifier: MIT
package dev.vault.core

/**
 * The foreground-service notification's text: app ids and origins only.
 * Never a pairing code, a device name (browser-supplied) or anything secret.
 */
data class SessionNotice(val title: String, val lines: List<String>) {
    /** The lines joined for the expanded notification. */
    val text: String get() = lines.joinToString("\n")

    companion object {
        /** What the lock screen shows instead (no app or origin). */
        const val PUBLIC_TITLE: String = "Vault session running"

        /** The notice for the vaults [serving] a browser and those still [pairing]. */
        fun of(serving: List<VaultIdentity>, pairing: List<VaultIdentity>): SessionNotice {
            val lines = serving.map(::line) + pairing.map { line(it) + " (pairing)" }
            val title = when {
                lines.isEmpty() -> "Vault sessions stopping"
                lines.size > 1 -> "${lines.size} vault sessions"
                serving.isNotEmpty() -> "Vault session running"
                else -> "Pairing a browser"
            }
            // A single pairing vault reads better without the suffix.
            val shown = if (serving.isEmpty() && pairing.size == 1) listOf(line(pairing[0])) else lines
            return SessionNotice(title, shown)
        }

        // Identities are validated ASCII; escaped anyway, as everything a browser named.
        private fun line(v: VaultIdentity) = "${Untrusted.display(v.appId)} · ${Untrusted.display(v.origin)}"
    }
}
