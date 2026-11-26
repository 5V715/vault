// SPDX-License-Identifier: MIT
package dev.vault.core

/** A vault's own settings. */
data class VaultSettings(
    /** Background replication only on unmetered networks. */
    val wifiOnly: Boolean = false,
)

/** The user typed the vault's app id exactly (delete confirmation). */
object DeleteConfirmation {
    fun matches(typed: String, appId: String): Boolean = typed == appId
}

/**
 * The settings files' plaintext (the app seals them under a background
 * Keystore key): `key=value` lines. Unknown keys are ignored and missing or
 * unparsable values fall back to the defaults, so an older or newer file never
 * stops the app.
 */
object Settings {
    private const val VERSION = "1"

    fun encode(s: LockSettings): ByteArray =
        KeyValues.encode(
            listOf(
                "v" to VERSION,
                "lockOnScreenOff" to s.lockOnScreenOff.toString(),
                "idleTimeoutMinutes" to s.idleTimeoutMinutes.toString(),
            ),
        )

    fun decodeLock(bytes: ByteArray): LockSettings {
        val m = KeyValues.decode(bytes).toMap()
        val d = LockSettings()
        return LockSettings(
            lockOnScreenOff = m["lockOnScreenOff"]?.toBooleanStrictOrNull() ?: d.lockOnScreenOff,
            idleTimeoutMinutes = m["idleTimeoutMinutes"]?.toIntOrNull() ?: d.idleTimeoutMinutes,
        )
    }

    fun encode(s: VaultSettings): ByteArray =
        KeyValues.encode(
            listOf(
                "v" to VERSION,
                "wifiOnly" to s.wifiOnly.toString(),
            ),
        )

    fun decodeVault(bytes: ByteArray): VaultSettings {
        val m = KeyValues.decode(bytes).toMap()
        return VaultSettings(wifiOnly = m["wifiOnly"]?.toBooleanStrictOrNull() ?: VaultSettings().wifiOnly)
    }
}

/** `key=value` lines; `\`, newline and CR in values are escaped. Keys repeat for lists. */
internal object KeyValues {
    fun encode(pairs: List<Pair<String, String>>): ByteArray =
        pairs.joinToString("") { (k, v) -> "$k=${escape(v)}\n" }.toByteArray(Charsets.UTF_8)

    fun decode(bytes: ByteArray): List<Pair<String, String>> =
        String(bytes, Charsets.UTF_8).split('\n').mapNotNull { line ->
            val eq = line.indexOf('=')
            if (eq <= 0) null else line.substring(0, eq) to unescape(line.substring(eq + 1))
        }

    private fun escape(v: String) = buildString {
        for (c in v) {
            when (c) {
                '\\' -> append("\\\\")
                '\n' -> append("\\n")
                '\r' -> append("\\r")
                else -> append(c)
            }
        }
    }

    private fun unescape(v: String) = buildString {
        var i = 0
        while (i < v.length) {
            val c = v[i]
            if (c == '\\' && i + 1 < v.length) {
                when (v[i + 1]) {
                    'n' -> append('\n')
                    'r' -> append('\r')
                    else -> append(v[i + 1])
                }
                i += 2
            } else {
                append(c)
                i++
            }
        }
    }
}
