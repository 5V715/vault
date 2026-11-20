// SPDX-License-Identifier: MIT
package dev.vault.core

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingCodeTest {
    @Test
    fun accepts_only_vault_pairing_codes() {
        assertTrue(PairingCode.isPairingCode("vault-pair:v1:abc"))
        assertTrue(PairingCode.isPairingCode("  vault-pair:v1:abc\n"))
        assertFalse(PairingCode.isPairingCode("vault-pair:v1:"))
        assertFalse(PairingCode.isPairingCode("vault-pair:v2:abc"))
        assertFalse(PairingCode.isPairingCode("https://example.com"))
        assertFalse(PairingCode.isPairingCode(""))
    }
}
