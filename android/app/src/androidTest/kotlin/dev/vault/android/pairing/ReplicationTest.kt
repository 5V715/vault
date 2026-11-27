// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.app.job.JobInfo
import android.app.job.JobScheduler
import android.content.Context
import android.net.NetworkCapabilities
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.vault.android.keys.AuthKeyInvalidatedException
import dev.vault.android.keys.VaultKeys
import dev.vault.android.replication.JobSchedulerJobs
import dev.vault.android.replication.Replicator
import dev.vault.android.replication.currentNetwork
import dev.vault.android.settings.SettingsStore
import dev.vault.android.storage.VaultDir
import dev.vault.android.vault.VaultBusyException
import dev.vault.android.vault.VaultManager
import dev.vault.core.JobKind
import dev.vault.core.LockSettings
import dev.vault.core.Network
import dev.vault.core.PairingMachine.State
import dev.vault.core.ReplicationJobIds
import dev.vault.core.ReplicationPolicy
import dev.vault.core.ReplicationPolicy.FollowUp
import dev.vault.core.ReplicationPolicy.Gate
import dev.vault.core.ReplicationStatus
import dev.vault.core.Trigger
import java.io.File
import java.security.KeyStore
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.vault_ffi.ReplicaTarget

/**
 * Background replication on the real JobScheduler (jobs scheduled through the
 * app's adapter and run through its job service; forced with `cmd jobscheduler
 * run -f` where the test can't wait), the sealed replica config and status,
 * Wi-Fi only, "replicate now", the run after a session (against the browser
 * stand-in), compaction and delete.
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

    private val scheduler: JobScheduler get() = h.context.getSystemService(Context.JOB_SCHEDULER_SERVICE) as JobScheduler

    private fun jobId(kind: JobKind) = ReplicationJobIds.of(h.dir.id, kind)

    private fun pending(kind: JobKind): JobInfo? = scheduler.getPendingJob(jobId(kind))

    /** As the system would start it (constraints and delays ignored). */
    private fun forceRun(kind: JobKind) {
        shell("cmd jobscheduler run -f ${h.context.packageName} ${jobId(kind)}")
    }

    private fun status(): ReplicationStatus = h.settings.status(h.dir)

    /** Files of the local replica (a directory standing in for S3/WebDAV). */
    private fun replicaFiles(): List<String> =
        h.replicaDir.walkTopDown().filter { it.isFile }.map { it.relativeTo(h.replicaDir).path }.sorted().toList()

    private fun addLocalReplica() = runBlocking { h.replicator.addReplica(h.dir, ReplicaTarget.Local(h.replicaDir.path)) }

    private fun JobInfo.unmeteredOnly(): Boolean =
        requiredNetwork?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) == true

    private fun awaitHandleClosed() = h.awaitTrue("the session's handle closed") {
        val lock = h.locks.of(h.dir.id)
        lock.tryLock().also { if (it) lock.unlock() }
    }

    @Test
    fun bg_replication_uses_manifest_key_only() {
        addLocalReplica()
        // The persisted periodic job carries nothing but the vault id.
        val job = h.awaitValue("the periodic job") { pending(JobKind.Periodic) }
        assertTrue(job.isPersisted)
        assertTrue(job.isPeriodic)
        assertEquals(ReplicationPolicy.PERIOD_SECS * 1000, job.intervalMillis)
        assertEquals(setOf(JobSchedulerJobs.EXTRA_VAULT), job.extras.keySet())
        assertEquals(h.dir.id, job.extras.getString(JobSchedulerJobs.EXTRA_VAULT))

        // The CRK can no longer be had: K_auth gone, the wrapped CRK garbage.
        KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.deleteEntry(h.keys.authAlias(h.dir.id))
        h.dir.wrappedCrk.writeBytes(ByteArray(60) { 7 })
        val aliasesBefore = h.keys.vaultIdsWithKeys()

        // The system runs the job (its non-exported service, bound as the system).
        forceRun(JobKind.Periodic)
        val s = h.awaitValue("a good run") { status().takeIf { it.state == ReplicationStatus.State.Ok } }
        assertNotNull(s.seq)
        assertTrue("the replica holds the vault: ${replicaFiles()}", replicaFiles().any { it.endsWith("manifest") })
        // Nothing asked for user presence or created a key on the way.
        assertEquals(aliasesBefore, h.keys.vaultIdsWithKeys())
        assertFalse(KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.containsAlias(h.keys.authAlias(h.dir.id)))
        // The replica config (and its target) is sealed, not in the clear.
        assertFalse(h.dir.sealed(VaultDir.KIND_REPLICAS).readBytes().containsBytes(h.replicaDir.path.toByteArray()))
    }

    @Test
    fun bg_replication_refuses_rollback() {
        // The container as created; a session then moves the vault (and its
        // high-water mark) forward. (The replica comes after, so the
        // after-session job, which the system may run at once, has nothing to push to.)
        val snapshot = File(h.context.cacheDir, "rollback-snapshot").apply { deleteRecursively() }
        assertTrue(h.dir.container.copyRecursively(snapshot))
        h.toRunning()
        h.standIn.ok("/put?k=newer&v=data")
        assertTrue(h.standIn.ok("/lock").startsWith("closed"))
        h.awaitServiceStopped()
        awaitHandleClosed()
        scheduler.cancelAll()
        // Someone puts the old container back (rollback).
        h.dir.container.deleteRecursively()
        assertTrue(snapshot.copyRecursively(h.dir.container))
        snapshot.deleteRecursively()
        addLocalReplica()

        val r = runBlocking { h.replicator.runAndFollowUp(h.dir.id, Trigger.Periodic) } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.NeedsAttention, r.status.state)
        assertTrue(r.status.blocked)
        assertTrue(r.status.problems.joinToString(), r.status.problems.any { it.contains("rolled back") })
        assertEquals("nothing pushed", emptyList<String>(), replicaFiles())
        // Never retried: no job queued, and the automatic runs don't touch it.
        assertNull(pending(JobKind.Retry))
        for (t in listOf(Trigger.Periodic, Trigger.AfterSession, Trigger.Retry)) {
            assertEquals(Replicator.Report.Skipped("needs attention"), runBlocking { h.replicator.runAndFollowUp(h.dir.id, t) })
        }
        assertEquals(r.status, status())
        // "Replicate now" may try again, but the rolled-back vault still pushes nothing.
        val manual = runBlocking { h.replicator.replicateNow(h.dir.id).await() } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.NeedsAttention, manual.status.state)
        assertNull(pending(JobKind.Retry))
        assertEquals(emptyList<String>(), replicaFiles())
    }

    @Test
    fun wifi_only_respected() {
        addLocalReplica()
        // The periodic job's network constraint follows the switch.
        assertFalse(h.awaitValue("the periodic job") { pending(JobKind.Periodic) }.unmeteredOnly())
        runBlocking { h.replicator.setWifiOnly(h.dir, true) }
        assertTrue(h.settings.vaultSettings(h.dir).wifiOnly)
        assertTrue(pending(JobKind.Periodic)!!.unmeteredOnly())

        // On a metered network a run waits for Wi-Fi: nothing is pushed, and a
        // retry is queued that only an unmetered network starts.
        val metered = Replicator(
            h.storage, h.manager, h.settings, h.locks, h.jobs,
            sessionActive = { false }, network = { Network.Metered }, deviceLocked = { false },
        )
        val waiting = runBlocking { metered.runAndFollowUp(h.dir.id, Trigger.Periodic) }
        assertEquals(
            Replicator.Report.Waiting(Gate.WaitForWifi, FollowUp.Retry(ReplicationPolicy.backoffSecs(0), wifiOnly = true)),
            waiting,
        )
        assertEquals(emptyList<String>(), replicaFiles())
        assertTrue(pending(JobKind.Retry)!!.unmeteredOnly())
        scheduler.cancel(jobId(JobKind.Retry))

        // The real system and network: a Wi-Fi-only job (not forced) runs only on an unmetered network.
        val real = currentNetwork(h.context)
        h.jobs.scheduleOnce(h.dir.id, JobKind.Retry, wifiOnly = true, delaySecs = 0)
        shell("cmd jobscheduler run ${h.context.packageName} ${jobId(JobKind.Retry)}")
        if (real == Network.Unmetered) {
            h.awaitValue("the Wi-Fi run") { status().takeIf { it.state == ReplicationStatus.State.Ok } }
        } else {
            Thread.sleep(5_000)
            assertEquals("not run on a $real network", ReplicationStatus.State.Never, status().state)
            assertEquals(emptyList<String>(), replicaFiles())
        }
        scheduler.cancel(jobId(JobKind.Retry))

        // "Replicate now" is the user's explicit request: it runs on a metered network.
        val now = runBlocking { metered.runAndFollowUp(h.dir.id, Trigger.Manual) } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.Ok, now.status.state)

        runBlocking { h.replicator.setWifiOnly(h.dir, false) }
        assertFalse(pending(JobKind.Periodic)!!.unmeteredOnly())
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
        // "Replicate now" (in the replicator's own scope) runs, succeeds and unblocks.
        val r = runBlocking { h.replicator.replicateNow(h.dir.id).await() } as Replicator.Report.Done
        assertEquals(ReplicationStatus.State.Ok, r.status.state)
        assertFalse(r.status.blocked)
        assertNotNull(r.status.lastOkMs)
        assertTrue(r.status.problems.isEmpty())
        assertTrue(replicaFiles().any { it.endsWith("manifest") })
        assertEquals(r.status, status())
        assertFalse(r.status.isStale(System.currentTimeMillis(), hasReplicas = true))
    }

    @Test
    fun replication_after_session_end() {
        addLocalReplica()
        val before = runBlocking { h.replicator.replicateNow(h.dir.id).await() } as Replicator.Report.Done
        val seq0 = checkNotNull(before.status.seq)
        scheduler.cancel(jobId(JobKind.AfterSession))

        h.toRunning()
        h.standIn.ok("/put?k=fresh&v=${enc("committed in the session")}")
        assertTrue(h.standIn.ok("/lock").startsWith("closed"))
        h.awaitServiceStopped()
        // The session's end queued its own job (the app's hooks); the system
        // may already run it, else it is forced here.
        h.awaitTrue("the after-session job") { pending(JobKind.AfterSession) != null || (status().seq ?: 0L) > seq0 }
        if (pending(JobKind.AfterSession) != null) forceRun(JobKind.AfterSession)
        val after = h.awaitValue("the replicated session") { status().takeIf { (it.seq ?: 0L) > seq0 } }
        assertEquals(ReplicationStatus.State.Ok, after.state)
    }

    @Test
    fun a_pairing_waits_for_a_replication_run_then_gets_the_vault() {
        addLocalReplica()
        // A run holds the vault (as replicate does while it uploads).
        val lock = h.locks.of(h.dir.id)
        assertTrue(lock.tryLock())
        try {
            h.scanBrowserCode()
            Thread.sleep(3_000)
            assertTrue("waits for the vault", h.coordinator.pairing.value?.state is State.Connecting)
        } finally {
            lock.unlock()
        }
        h.awaitState("the approval prompt") { it is State.AwaitingApproval }
        h.coordinator.deny()
        h.awaitEnded()
    }

    @Test
    fun compact_now_unlocks_only_for_compaction() {
        val seq0 = checkNotNull(runBlocking { h.manager.stats(h.dir) }.seq).toLong()
        val presence = PassingPresence()
        val seq = runBlocking { h.manager.compactNow(h.dir, presence) }
        assertTrue("$seq > $seq0", seq > seq0)
        assertEquals("one unlock prompt", 1, presence.prompts)
        // Locked again; the new mark is saved (asynchronously, after each commit).
        h.awaitTrue("the compacted mark") {
            val st = runBlocking { h.manager.stats(h.dir) }
            st.locked && st.seq?.toLong() == seq
        }

        // A vault that needs a re-wrap shows no prompt at all.
        assertTrue(h.dir.rewrapPending.createNewFile())
        val untouched = PassingPresence()
        try {
            runBlocking { h.manager.compactNow(h.dir, untouched) }
            fail("compacted a vault that needs a re-wrap")
        } catch (e: AuthKeyInvalidatedException) {
            assertEquals(0, untouched.prompts)
        }
        assertTrue(h.dir.rewrapPending.delete())

        // Refused while a session holds the vault.
        h.toRunning()
        val during = PassingPresence()
        try {
            runBlocking { h.manager.compactNow(h.dir, during) }
            fail("compacted during a session")
        } catch (e: VaultBusyException) {
            assertEquals(0, during.prompts)
        }
    }

    @Test
    fun lock_settings_persist_and_a_failed_read_is_not_cached() {
        val file = File(h.context.noBackupFilesDir, "app-settings.sealed")
        val saved = file.takeIf { it.isFile }?.readBytes()
        try {
            // Unreadable (as while the phone is locked): defaults for now, not remembered.
            file.writeBytes(ByteArray(40) { 1 })
            val store = SettingsStore(h.context, h.keys, h.storage)
            assertEquals(LockSettings(), store.lockSettings())
            assertFalse(store.lockSettingsKnown)
            val strict = LockSettings(lockOnScreenOff = true, idleTimeoutMinutes = 2)
            SettingsStore(h.context, h.keys, h.storage).saveLockSettings(strict)
            assertEquals("read again, not the cached defaults", strict, store.lockSettings())
            assertTrue(store.lockSettingsKnown)
            // A new process reads it back.
            assertEquals(strict, SettingsStore(h.context, h.keys, h.storage).lockSettings())
        } finally {
            if (saved != null) file.writeBytes(saved) else file.delete()
        }
    }

    @Test
    fun delete_vault_requires_auth_and_typed_app_id() {
        addLocalReplica()
        runBlocking { h.replicator.replicateNow(h.dir.id).await() }
        val replicated = replicaFiles()
        assertTrue(replicated.isNotEmpty())
        assertNotNull(pending(JobKind.Periodic))
        val lastRun0 = status().lastRunMs

        // While a session holds the vault: refused before any prompt.
        h.toRunning()
        val busy = PassingPresence()
        assertEquals(VaultManager.DeleteResult.Busy, runBlocking { h.manager.deleteConfirmed(h.dir, h.identity, APP, busy) })
        assertEquals(0, busy.confirms)
        assertTrue(h.standIn.ok("/lock").startsWith("closed"))
        h.awaitServiceStopped()
        awaitHandleClosed()
        // The session's end queues an after-session replication, which takes the vault's
        // lock while it runs (delete would rightly say Busy: CI run 46031365491, API 36).
        // Let it be queued or run, drop it, and wait until no run holds the vault.
        h.awaitTrue("the after-session job queued or run") {
            pending(JobKind.AfterSession) != null || status().lastRunMs != lastRun0
        }
        scheduler.cancel(jobId(JobKind.AfterSession))
        h.awaitTrue("no replication holds the vault") { !h.locks.of(h.dir.id).isLocked }

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

        // Typed and confirmed: the vault, its keys and (by the delete itself)
        // its jobs are gone; the replica is not touched.
        val ok = PassingPresence()
        assertEquals(VaultManager.DeleteResult.Deleted, runBlocking { h.manager.deleteConfirmed(h.dir, h.identity, APP, ok) })
        assertEquals(1, ok.confirms)
        assertEquals(0, ok.prompts)
        assertFalse(h.storage.exists(h.identity))
        assertFalse(h.dir.root.exists())
        assertFalse(h.keys.vaultIdsWithKeys().contains(h.dir.id))
        for (k in JobKind.entries) assertNull(k.name, pending(k))
        assertEquals(replicated, replicaFiles())
    }
}
