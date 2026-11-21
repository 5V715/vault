// SPDX-License-Identifier: MIT
package dev.vault.core

/** Scanned QR payloads: only `vault-pair:v1:` codes are pairing offers. */
object PairingCode {
    /** The prefix of every pairing offer (`vault_proto::QR_PREFIX`). */
    const val PREFIX: String = "vault-pair:v1:"

    /**
     * Longest accepted payload after the prefix. A maximal offer (512-byte relay
     * URL, 64-byte app id, 300-byte origin, a 32-byte key and length prefixes)
     * is under 1000 bytes, about 1300 base64url characters.
     */
    const val MAX_PAYLOAD: Int = 2048

    /** What a scanned text is, before vault-ffi parses the offer itself. */
    sealed interface Scan {
        /** A well-formed pairing code; pass [payload] to `VaultFactory.parseOffer`. */
        data class Offer(val payload: String) : Scan

        data object NotAVaultCode : Scan {
            val message = "Not a vault pairing code"
        }

        data object Malformed : Scan {
            val message = "This pairing code is damaged. Scan it again."
        }
    }

    /**
     * Classifies scanned [text]. Surrounding whitespace is dropped; the payload
     * must be non-empty unpadded base64url within [MAX_PAYLOAD]. Anything else
     * with the prefix is [Scan.Malformed], without it [Scan.NotAVaultCode].
     */
    fun classify(text: String): Scan {
        val t = text.trim()
        if (!t.startsWith(PREFIX)) return Scan.NotAVaultCode
        val payload = t.substring(PREFIX.length)
        val ok = payload.isNotEmpty() && payload.length <= MAX_PAYLOAD && payload.all { it.isBase64Url() }
        return if (ok) Scan.Offer(t) else Scan.Malformed
    }

    private fun Char.isBase64Url() =
        this in 'A'..'Z' || this in 'a'..'z' || this in '0'..'9' || this == '-' || this == '_'
}
