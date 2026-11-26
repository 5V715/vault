// SPDX-License-Identifier: MIT
package dev.vault.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SettingsTest {
    @Test
    fun lock_settings_round_trip_and_defaults() {
        val s = LockSettings(lockOnScreenOff = false, idleTimeoutMinutes = 42)
        assertEquals(s, Settings.decodeLock(Settings.encode(s)))
        // Missing or unknown keys fall back to the defaults; garbage is not a setting.
        assertEquals(LockSettings(), Settings.decodeLock(ByteArray(0)))
        assertEquals(LockSettings(), Settings.decodeLock("v=1\nfuture=1\n".toByteArray()))
        assertEquals(LockSettings(), Settings.decodeLock("v=1\nidleTimeoutMinutes=abc\n".toByteArray()))
        // Out-of-range values are clamped by LockSettings itself.
        assertEquals(
            LockSettings.MAX_IDLE_MINUTES * 60L,
            Settings.decodeLock("v=1\nidleTimeoutMinutes=99999\n".toByteArray()).idleTimeoutSecs,
        )
    }

    @Test
    fun vault_settings_round_trip_and_defaults() {
        val s = VaultSettings(wifiOnly = true)
        assertEquals(s, Settings.decodeVault(Settings.encode(s)))
        assertFalse(Settings.decodeVault(ByteArray(0)).wifiOnly)
        // A file from before compaction ran at once (it had a request flag) still reads.
        assertEquals(s, Settings.decodeVault("v=1\nwifiOnly=true\ncompactRequested=true\n".toByteArray()))
    }

    @Test
    fun delete_needs_the_exact_app_id() {
        assertTrue(DeleteConfirmation.matches("todo", "todo"))
        for (typed in listOf("", "tod", "Todo", " todo", "todo ", "todo\n")) {
            assertFalse(typed, DeleteConfirmation.matches(typed, "todo"))
        }
    }
}
