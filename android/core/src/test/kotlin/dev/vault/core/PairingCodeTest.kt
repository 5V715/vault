// SPDX-License-Identifier: MIT
package dev.vault.core

import org.junit.Assert.assertEquals
import org.junit.Test

class PairingCodeTest {
    private val payload = "vault-pair:v1:AQBodHRwczovL3JlbGF5LmV4YW1wbGUv_-"

    @Test
    fun qr_payload_validation() {
        // A pairing offer: the prefix plus base64url (no padding).
        assertEquals(PairingCode.Scan.Offer(payload), PairingCode.classify(payload))
        // Scanners often add a trailing newline; surrounding whitespace is dropped.
        assertEquals(PairingCode.Scan.Offer(payload), PairingCode.classify("  $payload\n"))

        val notVault = listOf(
            "",
            "https://example.com",
            "vault-pair:v2:AQB",
            "VAULT-PAIR:v1:AQB",
            "vault-recovery:v1:ABCD",
            "xvault-pair:v1:AQB",
        )
        for (t in notVault) assertEquals(t, PairingCode.Scan.NotAVaultCode, PairingCode.classify(t))

        // The prefix with a broken payload is a vault code, but a malformed one.
        val malformed = listOf(
            "vault-pair:v1:",
            "vault-pair:v1:AQB=",
            "vault-pair:v1:AQB+/",
            "vault-pair:v1:AQB AQB",
            "vault-pair:v1:AQB\nAQB",
            "vault-pair:v1:AQBé",
            "vault-pair:v1:" + "A".repeat(PairingCode.MAX_PAYLOAD + 1),
        )
        for (t in malformed) assertEquals(t, PairingCode.Scan.Malformed, PairingCode.classify(t))
        assertEquals(
            PairingCode.Scan.Offer("vault-pair:v1:" + "A".repeat(PairingCode.MAX_PAYLOAD)),
            PairingCode.classify("vault-pair:v1:" + "A".repeat(PairingCode.MAX_PAYLOAD)),
        )
        // Messages the scan screen shows.
        assertEquals("Not a vault pairing code", PairingCode.Scan.NotAVaultCode.message)
        assertEquals("This pairing code is damaged. Scan it again.", PairingCode.Scan.Malformed.message)
    }
}
