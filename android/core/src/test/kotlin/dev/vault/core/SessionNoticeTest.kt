// SPDX-License-Identifier: MIT
package dev.vault.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionNoticeTest {
    private fun id(app: String, origin: String) = (VaultIdentity.of(app, origin) as Checked.Valid).value

    @Test
    fun notice_shows_app_and_origin_only() {
        val todo = id("todo", "https://todo.example")
        val notes = id("notes", "https://notes.example:8443")
        val pairing = SessionNotice.of(serving = emptyList(), pairing = listOf(todo))
        assertEquals("Pairing a browser", pairing.title)
        assertEquals(listOf("todo · https://todo.example"), pairing.lines)

        val one = SessionNotice.of(serving = listOf(todo), pairing = emptyList())
        assertEquals("Vault session running", one.title)
        assertEquals(listOf("todo · https://todo.example"), one.lines)

        val two = SessionNotice.of(serving = listOf(todo), pairing = listOf(notes))
        assertEquals("2 vault sessions", two.title)
        assertEquals(listOf("todo · https://todo.example", "notes · https://notes.example:8443 (pairing)"), two.lines)
        assertEquals("todo · https://todo.example\nnotes · https://notes.example:8443 (pairing)", two.text)
    }

    @Test
    fun notice_for_nothing_says_stopping() {
        val n = SessionNotice.of(serving = emptyList(), pairing = emptyList())
        assertEquals("Vault sessions stopping", n.title)
        assertTrue(n.lines.isEmpty())
        // The lock screen sees a generic text only.
        assertFalse(SessionNotice.PUBLIC_TITLE.contains("todo"))
    }
}
