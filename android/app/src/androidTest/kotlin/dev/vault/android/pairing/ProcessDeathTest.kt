// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.app.Service
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.util.UUID
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * A real process death, driven from the host by android/ci/connected-test.sh
 * (never part of the Gradle run, which excludes this class):
 *
 * 1. [process_death_setup] leaves a running session, checks that the disk
 *    holds no secret in clear, signals "ready" through the browser and waits;
 * 2. the host kills the app process with `kill -9` (no onDestroy, no
 *    cleanup), then checks that the browser saw the session drop and that the
 *    system brought back neither the service nor its notification;
 * 3. [process_death_locks], in a new process, checks that nothing runs and
 *    that the next pairing needs a new unlock prompt.
 *
 * The instrumentation argument `processDeathPhase` names the phase; either
 * method fails without it, so neither can pass by accident in a normal run.
 */
@RunWith(AndroidJUnit4::class)
class ProcessDeathTest {
    private val h = PairingHarness()

    private fun requirePhase(phase: String) {
        val given = InstrumentationRegistry.getArguments().getString("processDeathPhase")
        assertEquals("run by android/ci/connected-test.sh with -e processDeathPhase $phase", phase, given)
    }

    @Test
    fun process_death_setup() {
        requirePhase("setup")
        keepScreenOn()
        // The system never restarts the service after the process is gone.
        assertEquals(Service.START_NOT_STICKY, SessionService.START_MODE)
        with(h) {
            open(fresh = true)
            toRunning()
            val marker = "process-death-" + UUID.randomUUID()
            standIn.ok("/put?k=secret&v=$marker")
            assertEquals(marker, standIn.ok("/get?k=secret"))

            // What a death leaves behind is what is on disk now: no CRK, no
            // vault data, no recovery key in clear anywhere in the app's data.
            val crk = runBlocking { manager.unwrapCrk(dir, PassingPresence()) }
            try {
                val files = context.dataDir.walkTopDown()
                    .onEnter { !java.nio.file.Files.isSymbolicLink(it.toPath()) }
                    .filter { it.isFile }
                    .toList()
                val vaultRoot = dir.root.canonicalPath + java.io.File.separator
                assertTrue(files.any { it.canonicalPath.startsWith(vaultRoot) })
                val needles = mapOf(
                    "the CRK" to crk,
                    "vault data" to marker.toByteArray(),
                    "the recovery key" to checkNotNull(recoveryKey).toByteArray(),
                )
                for (f in files) {
                    // A temporary file of an atomic write may be gone already;
                    // what it held is in its target.
                    val bytes = try {
                        f.readBytes()
                    } catch (e: java.io.FileNotFoundException) {
                        continue
                    }
                    for ((what, needle) in needles) {
                        assertFalse("$what in clear in ${f.relativeTo(context.dataDir)}", bytes.containsBytes(needle))
                    }
                }
            } finally {
                crk.fill(0)
            }

            // Tell the host, through the vault itself, that the session runs.
            standIn.ok("/put?k=phase&v=ready")
            Thread.sleep(KILL_WAIT_MS)
            throw AssertionError("the host did not kill the app process within ${KILL_WAIT_MS / 1000} s")
        }
    }

    @Test
    fun process_death_locks() {
        requirePhase("verify")
        keepScreenOn()
        // A new process: no service, no notification, no session.
        assertFalse(SessionService.running.value)
        with(h) {
            assertNull(sessionNotification())
            open(fresh = false)
            try {
                assertTrue(coordinator.running.value.isEmpty())
                // The browser lost the session with the process.
                assertTrue(standIn.ok("/state").startsWith("closed:"))
                // The next pairing needs a new unlock prompt; the committed data is there.
                val presence = PassingPresence()
                toRunning(presence)
                assertEquals(1, presence.prompts)
                assertEquals("ready", standIn.ok("/get?k=phase"))
                assertTrue(standIn.ok("/get?k=secret").startsWith("process-death-"))
            } finally {
                close()
            }
        }
    }

    private companion object {
        const val KILL_WAIT_MS = 120_000L
    }
}
