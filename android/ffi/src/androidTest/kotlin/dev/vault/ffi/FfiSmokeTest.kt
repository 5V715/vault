// SPDX-License-Identifier: MIT
package dev.vault.ffi

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.io.File
import java.util.UUID
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.vault_ffi.ApprovalFfi
import uniffi.vault_ffi.ApprovalRequestFfi
import uniffi.vault_ffi.ApproverCallback
import uniffi.vault_ffi.FactoryConfig
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.HighWaterStore
import uniffi.vault_ffi.SessionListener
import uniffi.vault_ffi.SessionStateFfi
import uniffi.vault_ffi.VaultFactory

/** The high-water blob in memory (the app seals it under K_bg in a file). */
private class MemoryHighWaterStore : HighWaterStore {
    @Volatile
    var saved: ByteArray? = null
    var saves = 0

    override fun load(): ByteArray? = saved?.copyOf()

    @Synchronized
    override fun save(state: ByteArray) {
        saved = state.copyOf()
        saves++
    }
}

/** Denies every prompt; the smoke test must never reach it. */
private class DenyingApprover : ApproverCallback {
    var asked = 0

    override suspend fun approve(request: ApprovalRequestFfi): ApprovalFfi {
        asked++
        return ApprovalFfi.Deny
    }
}

private object IgnoringListener : SessionListener {
    override fun onState(sessionId: ULong, state: SessionStateFfi) = Unit
}

/**
 * The native library loads through JNA on the device, and vault-ffi creates,
 * reopens and re-derives a vault in the app's no-backup directory.
 */
@RunWith(AndroidJUnit4::class)
class FfiSmokeTest {
    private lateinit var root: File

    @Before
    fun setUp() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        root = File(context.noBackupFilesDir, "ffi-smoke-${UUID.randomUUID()}")
        assertTrue(root.mkdirs())
    }

    @After
    fun tearDown() {
        root.deleteRecursively()
    }

    @Test
    fun ffi_loads_and_creates_vault() = runBlocking<Unit> {
        val dir = File(root, "vault").path
        val store = MemoryHighWaterStore()
        VaultFactory(FactoryConfig()).use { factory ->
            val created = factory.create(dir, APP_ID, ORIGIN, store)
            val crk = created.crk.copyOf()
            try {
                assertEquals(32, created.crk.size)
                assertEquals(32, created.manifestKey.size)
                assertNotNull("high-water saved before create returns", store.saved)
                created.handle.use { handle ->
                    assertFalse(handle.isLocked())
                    assertEquals(APP_ID, handle.binding().appId)
                    assertEquals(ORIGIN, handle.binding().origin)
                    val stats = handle.stats()
                    assertFalse(stats.locked)
                    assertEquals(0uL, stats.seq)
                }
            } finally {
                created.crk.fill(0)
                created.manifestKey.fill(0)
            }

            // Reopened from disk: locked, same binding, the saved mark is used.
            factory.open(dir, store).use { handle ->
                assertTrue(handle.isLocked())
                assertEquals(APP_ID, handle.binding().appId)
                assertTrue(handle.stats().locked)
                // Callback interfaces cross the boundary; a malformed code is
                // refused before any prompt.
                val approver = DenyingApprover()
                try {
                    handle.serveOffer("not a pairing code", approver, IgnoringListener)
                    fail("serveOffer accepted a malformed code")
                } catch (e: FfiException.BadOffer) {
                    // expected
                }
                assertEquals(0, approver.asked)
            }

            // The recovery key re-derives the same CRK from the stored container.
            val again = factory.rewrapWithRecoveryKey(dir, created.recoveryKey, store)
            try {
                assertArrayEquals(crk, again)
            } finally {
                again.fill(0)
                crk.fill(0)
            }
        }
        // A missing high-water mark fails closed.
        VaultFactory(FactoryConfig()).use { factory ->
            try {
                factory.open(dir, MemoryHighWaterStore()).close()
                fail("open without a high-water mark succeeded")
            } catch (e: FfiException.HighWaterMissing) {
                // expected
            }
        }
    }

    private companion object {
        const val APP_ID = "ffi-smoke"
        const val ORIGIN = "https://smoke.example"
    }
}
