// SPDX-License-Identifier: MIT
package dev.vault.core

/**
 * Text from the browser or the scanned code (device name, relay host, an
 * offer's app id and origin) as it may be shown: every character that is
 * invisible, reorders text or breaks lines is written as `\u{hex}`, the same
 * text vault-server's terminal approver prints (Rust `str::escape_debug`;
 * shared vectors in `vault-proto/tests/vectors/display-escape.tsv`), except
 * that quotes stay as they are.
 *
 * Escaped: control (Cc), format (Cf: bidi overrides and isolates, zero-width
 * characters, soft hyphen, BOM), private use (Co), unassigned (Cn), lone
 * surrogates, line and paragraph separators, every space but U+0020, a
 * combining mark at the very start (it would attach to the label before it),
 * and the backslash itself, so no input can pass for an escape. `\t`, `\n`,
 * `\r` and `\0` use their short forms.
 *
 * Unassigned is judged by this runtime's Unicode tables: an older Android
 * escapes characters newer than it knows, which errs on the safe side.
 */
object Untrusted {
    fun display(text: String): String {
        val out = StringBuilder(text.length)
        var i = 0
        while (i < text.length) {
            val cp = text.codePointAt(i)
            when (cp) {
                '\\'.code -> out.append("\\\\")
                '\t'.code -> out.append("\\t")
                '\n'.code -> out.append("\\n")
                '\r'.code -> out.append("\\r")
                0 -> out.append("\\0")
                else ->
                    if (hidden(cp) || (i == 0 && combining(cp))) {
                        out.append("\\u{").append(Integer.toHexString(cp)).append('}')
                    } else {
                        out.appendCodePoint(cp)
                    }
            }
            i += Character.charCount(cp)
        }
        return out.toString()
    }

    private fun hidden(cp: Int): Boolean = when (Character.getType(cp).toByte()) {
        Character.CONTROL,
        Character.FORMAT,
        Character.PRIVATE_USE,
        Character.UNASSIGNED,
        Character.SURROGATE,
        Character.LINE_SEPARATOR,
        Character.PARAGRAPH_SEPARATOR,
        -> true
        Character.SPACE_SEPARATOR -> cp != ' '.code
        else -> false
    }

    private fun combining(cp: Int): Boolean = when (Character.getType(cp).toByte()) {
        Character.NON_SPACING_MARK, Character.ENCLOSING_MARK -> true
        else -> false
    }
}
