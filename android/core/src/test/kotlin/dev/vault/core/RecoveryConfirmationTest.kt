// SPDX-License-Identifier: MIT
package dev.vault.core

import kotlin.random.Random
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class RecoveryConfirmationTest {
    // The format vault-container shows: 64 base32 characters in 16 groups of 4.
    private val key = "ABCD-EFGH-IJKL-MNOP-QRST-UVWX-YZ23-4567-AB2C-DE3F-GH4I-JK5L-MN6O-PQ7R-STUV-WXYZ"
    private val groups = key.split('-')

    @Test
    fun recovery_key_confirmation_4_of_16() {
        val c = RecoveryConfirmation.start(key, Random(7))!!
        assertEquals(4, c.indices.size)
        assertEquals(c.indices.sorted(), c.indices)
        assertEquals(4, c.indices.toSet().size)
        assertTrue(c.indices.all { it in 0 until 16 })

        val right = c.indices.map { groups[it] }
        assertTrue(c.check(right))
        // Typed like the parser accepts it: lower case, spaces, 0/1/8 for O/I/B.
        assertTrue(c.check(right.map { " " + it.lowercase().replace('O', '0').replace('I', '1').replace('B', '8') + " " }))
        // One wrong group, a missing one, the right groups in the wrong order.
        assertFalse(c.check(right.toMutableList().also { it[2] = "ZZZZ" }))
        assertFalse(c.check(right.take(3)))
        assertFalse(c.check(right.reversed()))
        assertFalse(c.check(right.map { "" }))
        // ASCII only, like the Rust parser: dotless ı (U+0131) is not I.
        val withI = (0 until 50).map { RecoveryConfirmation.start(key, Random(it))!! }
            .first { cc -> cc.indices.any { 'I' in groups[it] } }
        assertFalse(withI.check(withI.indices.map { groups[it].replace('I', '\u0131') }))
        assertTrue(withI.check(withI.indices.map { groups[it].replace('I', 'i') }))

        // Different randomness picks different groups (not always the same four).
        val picks = (0 until 50).map { RecoveryConfirmation.start(key, Random(it))!!.indices }.toSet()
        assertTrue(picks.size > 10)
        // Every group can be asked for.
        assertEquals((0 until 16).toSet(), picks.flatten().toSet())
    }

    @Test
    fun only_the_display_format_starts_a_confirmation() {
        assertNull(RecoveryConfirmation.start("", Random(1)))
        assertNull(RecoveryConfirmation.start(key.replace("-", ""), Random(1)))
        assertNull(RecoveryConfirmation.start(key.dropLast(5), Random(1)))
        assertNull(RecoveryConfirmation.start(key.replace('A', '1'), Random(1)))
        assertNull(RecoveryConfirmation.start("$key-ABCD", Random(1)))
    }
}
