// SPDX-License-Identifier: MIT
package dev.vault.core

/** Lock settings. Out-of-range values are clamped; idle locking cannot be turned off. */
data class LockSettings(
    /** Lock every vault when the screen turns off (default on). */
    val lockOnScreenOff: Boolean = true,
    val idleTimeoutMinutes: Int = DEFAULT_IDLE_MINUTES,
) {
    /**
     * For vault-server (`FactoryConfig.idleTimeoutSecs`): an approved session
     * ends after this long without a browser request. Relay pings do not count.
     */
    val idleTimeoutSecs: Long get() = idleTimeoutMinutes.coerceIn(1, MAX_IDLE_MINUTES) * 60L

    companion object {
        const val DEFAULT_IDLE_MINUTES: Int = 15
        const val MAX_IDLE_MINUTES: Int = 240
    }
}

/**
 * Decides when vaults lock and the foreground service stops. The service feeds
 * it session events; it returns [Action]s. Pure and single-threaded: call it
 * from one thread (the service's main thread).
 *
 * Rules:
 * - Any session end locks its vault, so no CRK stays in memory without a
 *   browser (this covers vault-server's idle timeout).
 * - Screen off locks everything when [LockSettings.lockOnScreenOff] is set, and
 *   always denies a pending approval.
 * - Stop locks everything.
 * - The service stops when no vault is pairing or serving.
 */
class SessionSupervisor(private val settings: () -> LockSettings) {
    sealed interface Action {
        /** Lock the vault: drop its CRK and server (ends its session). Idempotent. */
        data class Lock(val vaultId: String) : Action

        /** Deny the vault's pending approval prompt. */
        data class DenyPending(val vaultId: String) : Action

        data object StopService : Action
    }

    /** Vault id → its active session id, or null while pairing. Insertion order. */
    private val vaults = LinkedHashMap<String, Long?>()

    val isIdle: Boolean get() = vaults.isEmpty()

    /** A scan started pairing for [vaultId] (the service started). */
    fun onPairingStarted(vaultId: String) {
        if (vaultId !in vaults) vaults[vaultId] = null
    }

    fun onSessionActive(vaultId: String, sessionId: Long): List<Action> {
        vaults[vaultId] = sessionId
        return emptyList()
    }

    /** A pairing ended before its session became active (denied, expired, failed). */
    fun onPairingFinished(vaultId: String): List<Action> {
        if (vaultId !in vaults || vaults[vaultId] != null) return emptyList()
        vaults.remove(vaultId)
        return listOf(Action.Lock(vaultId)) + stopIfIdle()
    }

    fun onSessionEnded(vaultId: String, sessionId: Long, @Suppress("UNUSED_PARAMETER") reason: EndReason): List<Action> {
        if (vaults[vaultId] != sessionId) return emptyList()
        vaults.remove(vaultId)
        return listOf(Action.Lock(vaultId)) + stopIfIdle()
    }

    fun onScreenOff(): List<Action> =
        if (settings().lockOnScreenOff) {
            stopAll()
        } else {
            vaults.filterValues { it == null }.keys.map { Action.DenyPending(it) }
        }

    /** The notification's Stop action. */
    fun onStop(): List<Action> = stopAll()

    private fun stopAll(): List<Action> {
        if (vaults.isEmpty()) return emptyList()
        val out = vaults.filterValues { it == null }.keys.map<String, Action> { Action.DenyPending(it) } +
            vaults.keys.map { Action.Lock(it) } +
            Action.StopService
        vaults.clear()
        return out
    }

    private fun stopIfIdle(): List<Action> = if (vaults.isEmpty()) listOf(Action.StopService) else emptyList()
}
