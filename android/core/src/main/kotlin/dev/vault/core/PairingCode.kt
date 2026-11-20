// SPDX-License-Identifier: MIT
package dev.vault.core

/** Scanned QR payloads: only `vault-pair:v1:` codes are pairing offers. */
object PairingCode {
    /** The prefix of every pairing offer (`vault_proto::QR_PREFIX`). */
    const val PREFIX: String = "vault-pair:v1:"

    /**
     * Whether [text] looks like a pairing offer (the prefix plus a payload).
     * The payload itself is checked by vault-ffi (`parseOffer`).
     */
    fun isPairingCode(text: String): Boolean {
        val t = text.trim()
        return t.startsWith(PREFIX) && t.length > PREFIX.length
    }
}
