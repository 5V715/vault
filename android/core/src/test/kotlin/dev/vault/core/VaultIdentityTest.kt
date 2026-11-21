// SPDX-License-Identifier: MIT
package dev.vault.core

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class VaultIdentityTest {
    /** The shared vectors in `vault-proto/tests/vectors` (Rust is the reference). */
    private fun cases(name: String): List<Pair<String, String>> {
        val dir = System.getProperty("vault.vectors") ?: error("vault.vectors not set")
        val v = File(dir, name).readLines(Charsets.UTF_8)
            .filter { it.isNotEmpty() && !it.startsWith("#") }
            .map { line ->
                val tab = line.indexOf('\t')
                check(tab >= 0) { "input<TAB>expected: $line" }
                line.substring(0, tab) to line.substring(tab + 1)
            }
        assertTrue("vector file looks truncated", v.size > 10)
        return v
    }

    @Test
    fun vault_identity_normalized() {
        for ((input, expected) in cases("origins.tsv")) {
            val got = VaultIdentity.normalizeOrigin(input)
            if (expected == "!") {
                if (got is Checked.Valid) fail("$input must be rejected, got ${got.value}")
            } else {
                assertEquals(input, Checked.Valid(expected), got)
                assertEquals(Checked.Valid(expected), VaultIdentity.normalizeOrigin(expected))
            }
        }
        // An identity holds the canonical pair: these two inputs are one vault.
        val a = VaultIdentity.of("todo", "HTTPS://Todo.Example:443")
        val b = VaultIdentity.of("todo", "https://todo.example")
        assertEquals(b, a)
        assertEquals("https://todo.example", (a as Checked.Valid).value.origin)
        b as Checked.Valid
        // The LAN showcase and the deployed demo are separate vaults.
        val lan = VaultIdentity.of("todo", "https://192.168.1.20:8443") as Checked.Valid
        assertNotEquals(lan.value, b.value)
        // A different app on the same origin is a different vault too.
        val other = VaultIdentity.of("notes", "https://todo.example") as Checked.Valid
        assertNotEquals(other.value, b.value)
    }

    @Test
    fun app_ids_follow_the_shared_vectors() {
        assertTrue(VaultIdentity.validateAppId("") is Checked.Invalid)
        for ((input, expected) in cases("app-ids.tsv")) {
            assertEquals(input, expected == "ok", VaultIdentity.validateAppId(input) is Checked.Valid)
        }
        assertTrue(VaultIdentity.of("Todo", "https://todo.example") is Checked.Invalid)
        assertTrue(VaultIdentity.of("todo", "http://todo.example") is Checked.Invalid)
    }

    @Test
    fun invalid_origins_say_why() {
        val r = VaultIdentity.normalizeOrigin("http://todo.example") as Checked.Invalid
        assertTrue(r.reason, r.reason.contains("localhost"))
    }
}
