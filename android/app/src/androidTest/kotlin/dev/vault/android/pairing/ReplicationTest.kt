// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.work.NetworkType
import androidx.work.WorkInfo
import androidx.work.WorkManager
import androidx.work.testing.WorkManagerTestInitHelper
import dev.vault.android.keys.VaultKeys
import dev.vault.android.replication.ReplicationScheduler
import dev.vault.android.replication.Replicator
import dev.vault.android.replication.Trigger
import dev.vault.android.storage.VaultDir
import dev.vault.android.vault.VaultManager
import dev.vault.core.Network
import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationStatus
import java.io.File
import java.security.KeyStore
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.vault_ffi.ReplicaTarget

/**
 * Background replication: the WorkManager jobs (a test WorkManager whose
 * driver decides when constraints are met), the replicator they run, the
 * sealed replica config and status, Wi-Fi only, "replicate now", the run after
 * a session (against the browser stand-in) and delete.
 */
@RunWith(AndroidJUnit4::class)
class ReplicationTest {
    private val h = PairingHarness()

    companion object {
        @BeforeClass
        @JvmStatic
        fun awake() = keepScreenOn()
    }

    @Before
    fun setUp() = h.open(fresh = true, replication = true)

    @After
    fun tearDown() = h.close()

    private val work: WorkManager get() = WorkManager.getInstance(h.context)

    private fun infos(name: String): List<WorkInfo> = work.getWorkInfosForUniqueWork(name).get()

    private fun pending(name: String): WorkInfo? = infos(name).firstOrNull { it.state == WorkInfo.State.ENQUEUED }

    private fun status(): ReplicationStatus = h.settings.status(h.dir)

    /** Files of the local replica (a directory standing in for S3/WebDAV). */
    private fun replicaFiles(): List<String> =
        h.replicaDir.walkTopDown().filter { it.isFile }.map { it.relativeTo(h.replicaDir).path }.sorted().toList()

    private fun addLocalReplica() = runBlocking { h.replicator.addReplica(h.dir, ReplicaTarget.Local(h.replicaDir.path)) }

    private fun runJob(name: String, periodic: Boolean) {
        val info = h.awaitValue("work $name") { pending(name) }
        val driver = checkNotNull(WorkManagerTestInitHelper.getTestDriver(h.context))
        driver.setAllConstraintsMet(info.id)
        if (periodic) driver.setPeriodDelayMet(info.id) else driver.setInitialDelayMet(info.id)
    }

    @Test
    fun bg_replication_uses_manifest_key_only() {
        addLocalReplica()
        // The CRK can no longer be had: K_auth gone, the wrapped CRK garbage.
        KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.deleteEntry(h.keys.authAlias(h.dir.id))
        h.dir.wrappedCrk.writeBytes(ByteArray(60) { 7 })
        val aliasesBefore = h.keys.vaultIdsWithKeys()

        // The periodic job (as the system would start it) replicates anyway.
        runJob(ReplicationScheduler.periodicName(h.dir.id), periodic = true)
        val s = h.awaitValue("a good run") { status().takeIf { it.state == ReplicationStatus.State.Ok } }
        assertNotNull(s.seq)
        assertTrue("the replica holds the vault: ${replicaFiles()}", replicaFiles().any { it.endsWith("manifest") })
        // Nothing asked for user presence or created a key on the way.
        assertEquals(aliasesBefore, h.keys.vaultIdsWithKeys())
        assertFalse(KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.containsAlias(h.keys.authAlias(h.dir.id)))
        // The replica config (and its target) is sealed, not in the clear.
        val sealed = h.dir.sealed(VaultDir.KIND_REPLICAS).readBytes()
        assertFalse(sealed.containsBytes(h.replicaDir.path.toByteArray()))
    }

    @Test
    fun bg_replication_refuses_rollback() {
        addLocalReplica()
        // The container as created; a session then moves the vault (and its
        // high-water mark) forward.
        val snapshot = File(h.context.cacheDir, "rollback-snapshot").apply { deleteRecursively() }
        assertTrue(h.dir.container.copyRecursively(snapshot))
        h.toRunning()
        h.standIn.ok("/put?k=newer&v=data")
        assertTrue(h.standIn.ok("/lock").startsWith("closed"))
        h.awaitServiceStopped()
        work.cancelAllWork().result.get()
        // Someone puts the old container back (rollback).
        h.awaitTrue("the session's handle closed") { runBlocking { !h.coordinator.holds(h.dir.id) } && h.locks.of(h.dir.id).tryLock().also { if (it) h.locks.of(h.dir.id).unlock() } }
        h.dir.container.deleteRecursively()
        assertTrue(snapshot.copyRecursively(h.dir.container))
        snapshot.deleteRecursively()

        val r = runBlocking { h.replicator.run(h.dir.id, Trigger.Periodic) } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.NeedsAttention, r.status.state)
        assertTrue(r.status.blocked)
        assertTrue(r.status.problems.joinToString(), r.status.problems.any { it.contains("rolled back") })
        assertEquals("nothing pushed", emptyList<String>(), replicaFiles())
        // Never retried: no job scheduled, and the next automatic runs don't touch it.
        assertEquals(null, pending(ReplicationScheduler.onceName(h.dir.id)))
        for (t in listOf(Trigger.Periodic, Trigger.AfterSession, Trigger.Retry)) {
            assertEquals(Replicator.Report.Skipped("needs attention"), runBlocking { h.replicator.run(h.dir.id, t) })
        }
        assertEquals(r.status, status())
        assertEquals(emptyList<String>(), replicaFiles())
    }

    @Test
    fun wifi_only_respected() {
        addLocalReplica()
        val periodic = ReplicationScheduler.periodicName(h.dir.id)
        assertEquals(NetworkType.CONNECTED, pending(periodic)!!.constraints.requiredNetworkType)
        runBlocking { h.replicator.setWifiOnly(h.dir, true) }
        assertEquals(NetworkType.UNMETERED, h.awaitValue("updated work") { pending(periodic) }.constraints.requiredNetworkType)
        assertTrue(h.settings.vaultSettings(h.dir).wifiOnly)

        // On a metered network the job waits for Wi-Fi: nothing is pushed, a
        // run is queued for an unmetered network.
        val metered = Replicator(
            h.storage, h.manager, h.settings, h.locks, h.scheduler,
            sessionActive = { false }, network = { Network.Metered }, deviceLocked = { false },
        )
        work.cancelUniqueWork(ReplicationScheduler.onceName(h.dir.id)).result.get()
        assertEquals(Replicator.Report.Waiting(Gate.WaitForWifi), runBlocking { metered.run(h.dir.id, Trigger.Periodic) })
        assertEquals(emptyList<String>(), replicaFiles())
        val queued = h.awaitValue("the Wi-Fi run") { pending(ReplicationScheduler.onceName(h.dir.id)) }
        assertEquals(NetworkType.UNMETERED, queued.constraints.requiredNetworkType)
        // "Replicate now" is the user's explicit request: it runs on the metered network.
        val now = runBlocking { metered.run(h.dir.id, Trigger.Manual) } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.Ok, now.status.state)

        runBlocking { h.replicator.setWifiOnly(h.dir, false) }
        assertEquals(NetworkType.CONNECTED, h.awaitValue("updated work") { pending(periodic) }.constraints.requiredNetworkType)
    }

    @Test
    fun replicate_now() {
        addLocalReplica()
        // Blocked (e.g. after a rollback the user then fixed): automatic runs skip it.
        h.settings.saveStatus(
            h.dir,
            ReplicationStatus(state = ReplicationStatus.State.NeedsAttention, blocked = true, problems = listOf("earlier problem")),
        )
        assertEquals(Replicator.Report.Skipped("needs attention"), runBlocking { h.replicator.run(h.dir.id, Trigger.Periodic) })
        assertEquals(emptyList<String>(), replicaFiles())
        // "Replicate now" runs, succeeds and unblocks.
        val r = runBlocking { h.replicator.run(h.dir.id, Trigger.Manual) } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.Ok, r.status.state)
        assertFalse(r.status.blocked)
        assertNotNull(r.status.lastOkMs)
        assertTrue(r.status.problems.isEmpty())
        assertTrue(replicaFiles().any { it.endsWith("manifest") })
        assertEquals(r.status, status())
    }

    @Test
    fun replication_after_session_end() {
        addLocalReplica()
        val before = runBlocking { h.replicator.run(h.dir.id, Trigger.Manual) } as Replicator.Report.Done
        val seq0 = checkNotNull(before.status.seq)
        work.cancelUniqueWork(ReplicationScheduler.onceName(h.dir.id)).result.get()

        h.toRunning()
        h.standIn.ok("/put?k=fresh&v=${enc("committed in the session")}")
        assertTrue(h.standIn.ok("/lock").startsWith("closed"))
        h.awaitServiceStopped()
        // The session's end queued a run; the system runs it once there is a network.
        runJob(ReplicationScheduler.onceName(h.dir.id), periodic = false)
        val after = h.awaitValue("the replicated session") { status().takeIf { (it.seq ?: 0L) > seq0 } }
        assertEquals(ReplicationStatus.State.Ok, after.state)
    }

    @Test
    fun delete_vault_requires_auth_and_typed_app_id() {
        addLocalReplica()
        runBlocking { h.replicator.run(h.dir.id, Trigger.Manual) }
        val replicated = replicaFiles()
        assertTrue(replicated.isNotEmpty())

        // A mistyped app id: refused before any prompt.
        for (typed in listOf("", "tod", "Todo", " todo")) {
            val p = PassingPresence()
            assertEquals(VaultManager.DeleteResult.AppIdNotTyped, runBlocking { h.manager.deleteConfirmed(h.dir, h.identity, typed, p) })
            assertEquals(0, p.confirms)
        }
        // The prompt declined: still there.
        val declined = PassingPresence(pass = false)
        assertEquals(VaultManager.DeleteResult.NotConfirmed, runBlocking { h.manager.deleteConfirmed(h.dir, h.identity, APP, declined) })
        assertEquals(1, declined.confirms)
        assertTrue(h.storage.exists(h.identity))

        // Typed and confirmed: the vault and its keys are gone, its jobs too;
        // the replica is not touched.
        val ok = PassingPresence()
        assertEquals(VaultManager.DeleteResult.Deleted, runBlocking { h.manager.deleteConfirmed(h.dir, h.identity, APP, ok) })
        assertEquals(1, ok.confirms)
        assertEquals(0, ok.prompts)
        h.replicator.forget(h.dir.id)
        assertFalse(h.storage.exists(h.identity))
        assertFalse(h.dir.root.exists())
        assertFalse(h.keys.vaultIdsWithKeys().contains(h.dir.id))
        h.awaitTrue("the jobs cancelled") { pending(ReplicationScheduler.periodicName(h.dir.id)) == null }
        assertEquals(replicated, replicaFiles())
    }
}
