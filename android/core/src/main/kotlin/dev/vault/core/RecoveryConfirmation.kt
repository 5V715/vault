// SPDX-License-Identifier: MIT
package dev.vault.core

import java.security.MessageDigest
import kotlin.random.Random

/**
 * The "did you write it down?" check after creating a vault: the user re-enters
 * 4 random groups of the 16-group recovery key.
 *
 * The groups are kept only as long as this object lives (the create screen);
 * the recovery key itself arrives from vault-ffi as a `String`, which the JVM
 * cannot wipe, so drop every reference as soon as the check passes.
 */
class RecoveryConfirmation private constructor(
    private val groups: List<String>,
    /** The 0-based groups to ask for, ascending. */
    val indices: List<Int>,
) {
    /**
     * Whether [answers] (one per index, in order) match. Typed text is
     * normalized like the recovery-key parser: case, spaces and dashes are
     * ignored and `0`, `1`, `8` read as `O`, `I`, `B`.
     */
    fun check(answers: List<String>): Boolean {
        if (answers.size != indices.size) return false
        var ok = true
        for ((i, idx) in indices.withIndex()) {
            val typed = normalize(answers[i]).toByteArray(Charsets.US_ASCII)
            val want = groups[idx].toByteArray(Charsets.US_ASCII)
            ok = MessageDigest.isEqual(typed, want) and ok
        }
        return ok
    }

    companion object {
        const val GROUPS: Int = 16
        const val ASKED: Int = 4
        private val DISPLAY = Regex("^([A-Z2-7]{4}-){15}[A-Z2-7]{4}$")

        /** Starts a check for [display] (vault-container's 16×4 form), or null if malformed. */
        fun start(display: String, random: Random): RecoveryConfirmation? {
            if (!DISPLAY.matches(display)) return null
            val indices = (0 until GROUPS).shuffled(random).take(ASKED).sorted()
            return RecoveryConfirmation(display.split('-'), indices)
        }

        private fun normalize(text: String): String = buildString {
            for (c in text) {
                when {
                    c == '-' || c.isWhitespace() -> {}
                    c == '0' -> append('O')
                    c == '1' -> append('I')
                    c == '8' -> append('B')
                    // ASCII only, like the Rust parser: no Unicode case folding (ı → I).
                    c in 'a'..'z' -> append(c - 32)
                    else -> append(c)
                }
            }
        }
    }
}
