// SPDX-License-Identifier: MIT
package dev.vault.core

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class UntrustedTest {
    /** The vectors' input notation: `\u{hex}` is that code point, `\\` a backslash. */
    private fun decode(s: String): String {
        val out = StringBuilder()
        var i = 0
        while (i < s.length) {
            val c = s[i]
            if (c != '\\') {
                out.append(c)
                i++
                continue
            }
            when {
                s.startsWith("\\\\", i) -> {
                    out.append('\\')
                    i += 2
                }
                s.startsWith("\\u{", i) -> {
                    val end = s.indexOf('}', i)
                    check(end > 0) { "unterminated \\u{ in $s" }
                    out.appendCodePoint(s.substring(i + 3, end).toInt(16))
                    i = end + 1
                }
                else -> error("bad escape in vector: $s")
            }
        }
        return out.toString()
    }

    /** `vault-proto/tests/vectors/display-escape.tsv`, also pinned by vault-server's approver. */
    @Test
    fun untrusted_text_is_escaped_like_the_rust_approver() {
        val dir = System.getProperty("vault.vectors") ?: error("vault.vectors not set")
        val cases = File(dir, "display-escape.tsv").readLines(Charsets.UTF_8)
            .filter { it.isNotEmpty() && !it.startsWith("#") }
        assertTrue("vector file looks truncated", cases.size > 20)
        for (line in cases) {
            val tab = line.indexOf('\t')
            check(tab >= 0) { "input<TAB>expected: $line" }
            val input = line.substring(0, tab)
            assertEquals(input, line.substring(tab + 1), Untrusted.display(decode(input)))
        }
    }

    @Test
    fun escaped_text_has_no_invisible_or_reordering_characters_left() {
        val nasty = "‮⁦​­﻿\u0000\u001b "
        val shown = Untrusted.display("x$nasty")
        assertTrue(shown, shown.all { it.code in 0x20..0x7e })
        // Escaping is idempotent only through the backslash: no input can pass for an escape.
        assertEquals("\\\\u{202e}", Untrusted.display("\\u{202e}"))
    }
}
