// SPDX-License-Identifier: MIT
package dev.vault.core

/** A checked value: [Valid] or [Invalid] with a reason to show the user. */
sealed interface Checked<out T> {
    data class Valid<T>(val value: T) : Checked<T>

    data class Invalid(val reason: String) : Checked<Nothing>
}

/**
 * Which vault serves an app: the exact normalized `(appId, origin)` pair. Two
 * identities are the same vault only when both strings are equal, so the LAN
 * showcase (`https://192.168.x.y:8443`) and a deployed demo are separate vaults.
 *
 * The rules are a port of `vault_proto::binding` (the reference): the shared
 * vectors in `vault-proto/tests/vectors/` run against both. Construct only
 * through [of].
 */
class VaultIdentity private constructor(val appId: String, val origin: String) {
    override fun equals(other: Any?): Boolean =
        other is VaultIdentity && other.appId == appId && other.origin == origin

    override fun hashCode(): Int = 31 * appId.hashCode() + origin.hashCode()

    override fun toString(): String = "VaultIdentity($appId, $origin)"

    companion object {
        const val MAX_APP_ID: Int = 64
        const val MAX_ORIGIN: Int = 300

        /** Validates [appId] and normalizes [origin] into one identity. */
        fun of(appId: String, origin: String): Checked<VaultIdentity> {
            val id = validateAppId(appId)
            if (id is Checked.Invalid) return id
            return when (val o = normalizeOrigin(origin)) {
                is Checked.Invalid -> o
                is Checked.Valid -> Checked.Valid(VaultIdentity(appId, o.value))
            }
        }

        /** `appId` must match `^[a-z0-9][a-z0-9.-]{0,63}$`. */
        fun validateAppId(appId: String): Checked<String> {
            val ok = appId.isNotEmpty() &&
                appId.length <= MAX_APP_ID &&
                (appId[0].isLowerAscii() || appId[0].isDigitAscii()) &&
                appId.all { it.isLowerAscii() || it.isDigitAscii() || it == '.' || it == '-' }
            return if (ok) {
                Checked.Valid(appId)
            } else {
                Checked.Invalid("app id must be 1–64 of a–z, 0–9, '.', '-', starting with a letter or digit")
            }
        }

        /** Checks [origin] against the binding rules and returns its canonical form. */
        fun normalizeOrigin(origin: String): Checked<String> {
            fun bad(why: String) = Checked.Invalid("invalid origin: $why")
            if (origin.length > MAX_ORIGIN) return bad("too long")
            if (origin.any { it.code > 0x7f }) return bad("non-ASCII character (use punycode for the host)")
            if (origin.any { it.code <= 0x20 || it.code == 0x7f || it == '\\' }) return bad("forbidden character")
            val sep = origin.indexOf("://")
            if (sep < 0) return bad("missing scheme")
            val scheme = origin.substring(0, sep).lowerAscii()
            val authority = origin.substring(sep + 3)
            val (secure, defaultPort) = when (scheme) {
                "https" -> true to 443
                "http" -> false to 80
                else -> return bad("scheme must be https (or http for loopback)")
            }
            if (authority.any { it == '/' || it == '?' || it == '#' }) return bad("path, query or fragment")
            if (authority.contains('@')) return bad("userinfo")
            val host: String
            val portText: String?
            if (authority.startsWith("[")) {
                val close = authority.indexOf(']')
                if (close < 0) return bad("bad IPv6 literal")
                val h = authority.substring(1, close)
                val tail = authority.substring(close + 1)
                portText = when {
                    tail.isEmpty() -> null
                    tail.startsWith(":") -> tail.substring(1)
                    else -> return bad("bad authority")
                }
                if (h.isEmpty() || !h.all { it.isHexAscii() || it == ':' || it == '.' }) {
                    return bad("bad IPv6 literal")
                }
                host = "[" + h.lowerAscii() + "]"
            } else {
                val colon = authority.indexOf(':')
                val h = (if (colon < 0) authority else authority.substring(0, colon)).lowerAscii()
                portText = if (colon < 0) null else authority.substring(colon + 1)
                val labelsOk = h.split('.').all { l ->
                    l.isNotEmpty() && l.length <= 63 &&
                        l.all { it.isLowerAscii() || it.isDigitAscii() || it == '-' }
                }
                if (h.isEmpty() || h.length > 253 || !labelsOk) return bad("bad host")
                host = h
            }
            val port: Int? = when (portText) {
                null -> null
                else -> {
                    val n = if (portText.isNotEmpty() && portText.length <= 5 && portText.all { it.isDigitAscii() }) {
                        portText.toInt().takeIf { it in 1..65535 }
                    } else {
                        null
                    }
                    n ?: return bad("bad port")
                    n.takeIf { it != defaultPort }
                }
            }
            if (!secure && host != "localhost" && host != "127.0.0.1" && host != "[::1]") {
                return bad("http is only allowed for localhost, 127.0.0.1 and [::1]")
            }
            return Checked.Valid(if (port != null) "$scheme://$host:$port" else "$scheme://$host")
        }

        private fun Char.isLowerAscii() = this in 'a'..'z'

        private fun Char.isDigitAscii() = this in '0'..'9'

        private fun Char.isHexAscii() = isDigitAscii() || this in 'a'..'f' || this in 'A'..'F'

        private fun String.lowerAscii() = String(CharArray(length) { i -> this[i].let { if (it in 'A'..'Z') it + 32 else it } })
    }
}
