// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.SessionSupervisor.Action
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionSupervisorTest {
    private val a = "vault-a"
    private val b = "vault-b"

    @Test
    fun idle_timeout_locks() {
        val s = SessionSupervisor { LockSettings() }
        s.onPairingStarted(a)
        assertEquals(emptyList<Action>(), s.onSessionActive(a, 1))
        s.onPairingStarted(b)
        s.onSessionActive(b, 2)
        // vault-server ends an idle session; that vault locks (drops its CRK), the
        // other keeps serving, so the service stays.
        assertEquals(listOf(Action.Lock(a)), s.onSessionEnded(a, 1))
        // The last session ending locks and stops the service.
        assertEquals(listOf(Action.Lock(b), Action.StopService), s.onSessionEnded(b, 2))
        // The end that the lock itself causes is a no-op.
        assertEquals(emptyList<Action>(), s.onSessionEnded(b, 2))

        // The idle timeout handed to vault-server (FactoryConfig.idleTimeoutSecs).
        assertEquals(15L * 60, LockSettings().idleTimeoutSecs)
        assertEquals(5L * 60, LockSettings(idleTimeoutMinutes = 5).idleTimeoutSecs)
        // Out-of-range settings are clamped, never "off".
        assertEquals(60L, LockSettings(idleTimeoutMinutes = 0).idleTimeoutSecs)
        assertEquals(LockSettings.MAX_IDLE_MINUTES * 60L, LockSettings(idleTimeoutMinutes = 100_000).idleTimeoutSecs)
    }

    @Test
    fun any_session_end_locks_its_vault() {
        // An unlocked vault without a session would keep its CRK in memory.
        val one = SessionSupervisor { LockSettings() }
        one.onPairingStarted(a)
        one.onSessionActive(a, 1)
        // A stale session id changes nothing.
        assertEquals(emptyList<Action>(), one.onSessionEnded(a, 99))
        assertEquals(listOf(Action.Lock(a), Action.StopService), one.onSessionEnded(a, 1))
        // A late SessionActive after a stop leaves no ghost entry.
        one.onSessionActive(a, 2)
        assertTrue(one.isIdle)
        // A pairing that never became active: no session, nothing unlocked, but
        // the service started at scan time must stop.
        val s = SessionSupervisor { LockSettings() }
        s.onPairingStarted(a)
        assertEquals(listOf(Action.Lock(a), Action.StopService), s.onPairingFinished(a))
    }

    @Test
    fun screen_off_lock_rule() {
        var settings = LockSettings(lockOnScreenOff = true)
        val s = SessionSupervisor { settings }
        s.onPairingStarted(a)
        s.onSessionActive(a, 1)
        s.onPairingStarted(b)
        s.onSessionActive(b, 2)
        assertEquals(listOf(Action.Lock(a), Action.Lock(b), Action.StopService), s.onScreenOff())
        assertTrue(s.isIdle)

        // Off: sessions keep running with the screen off.
        settings = LockSettings(lockOnScreenOff = false)
        s.onPairingStarted(a)
        s.onSessionActive(a, 3)
        assertEquals(emptyList<Action>(), s.onScreenOff())
        // ...but a pending approval never survives the screen going off.
        s.onPairingStarted(b)
        assertEquals(listOf(Action.DenyPending(b)), s.onScreenOff())

        // Nothing running: nothing to do.
        assertEquals(emptyList<Action>(), SessionSupervisor { LockSettings() }.onScreenOff())
    }

    @Test
    fun stop_locks_everything() {
        val s = SessionSupervisor { LockSettings(lockOnScreenOff = false) }
        s.onPairingStarted(a)
        s.onSessionActive(a, 1)
        s.onPairingStarted(b)
        assertEquals(
            listOf(Action.DenyPending(b), Action.Lock(a), Action.Lock(b), Action.StopService),
            s.onStop(),
        )
        assertTrue(s.isIdle)
        // Stop is the kill switch: it stops the service even when nothing is tracked.
        assertEquals(listOf(Action.StopService), s.onStop())
    }
}
