// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.app.Service
import android.content.ComponentName
import android.content.Context
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.SystemClock
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import dev.vault.android.MainActivity
import dev.vault.android.VaultApp
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.ApprovalRequest
import dev.vault.core.Checked
import dev.vault.core.EndReason
import dev.vault.core.LockSettings
import dev.vault.core.PairingMachine.State
import dev.vault.core.PairingOutcome
import dev.vault.core.VaultIdentity
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import java.util.UUID
import javax.crypto.Cipher
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.AfterClass
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The test stand-in for the user at the BiometricPrompt: test keys
 * (`VaultKeys(requireAuth = false)`) need no authentication, so it passes the
 * CryptoObject's cipher back unchanged, or cancels ([pass] = false). It exists
 * only in androidTest; the app's approver is the real one.
 */
private class PassingPresence(private val pass: Boolean = true) : UserPresence {
    @Volatile
    var prompts = 0

    override suspend fun authenticate(title: String, cipher: Cipher): Cipher? {
        prompts++
        return cipher.takeIf { pass }
    }

    override suspend fun confirm(title: String): Boolean = pass
}

/**
 * The browser stand-in's control API (vault-client/examples/browser_stand_in.rs),
 * which CI runs on the host and forwards into the emulator with `adb reverse`.
 */
private class StandIn(private val port: Int) {
    fun get(path: String, timeoutMs: Int = 45_000): Pair<Int, String> {
        val c = URL("http://127.0.0.1:$port$path").openConnection() as HttpURLConnection
        c.connectTimeout = 5_000
        c.readTimeout = timeoutMs
        try {
            val code = c.responseCode
            val body = (if (code < 400) c.inputStream else c.errorStream)?.use { it.readBytes().toString(Charsets.UTF_8) }.orEmpty()
            return code to body
        } finally {
            c.disconnect()
        }
    }

    fun ok(path: String): String {
        val (code, body) = get(path)
        assertEquals("stand-in $path -> $body", 200, code)
        return body
    }
}

private fun enc(s: String) = URLEncoder.encode(s, "UTF-8")

private fun ByteArray.contains(needle: ByteArray): Boolean {
    if (needle.isEmpty() || needle.size > size) return false
    outer@ for (i in 0..size - needle.size) {
        for (j in needle.indices) if (this[i + j] != needle[j]) continue@outer
        return true
    }
    return false
}

/**
 * Pairing against a real relay and a browser stand-in (vault-client, native):
 * the scan, the foreground service, the approval with the CRK unwrap, the
 * session, Stop and what a process death leaves behind.
 *
 * Needs the stand-in: CI starts it on the host and passes its control port as
 * the instrumentation argument `standIn` (adb reverse makes 127.0.0.1 reach
 * the host). Emulators can't pass a BiometricPrompt, so the vault uses test
 * keys and [PassingPresence] stands in for the user at the prompt.
 */
@RunWith(AndroidJUnit4::class)
class PairingTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context: Context = instrumentation.targetContext
    private val app = context.applicationContext as VaultApp
    private val identity = (VaultIdentity.of(APP, ORIGIN) as Checked.Valid).value
    private lateinit var standIn: StandIn
    private lateinit var keys: VaultKeys
    private lateinit var storage: VaultStorage
    private lateinit var manager: VaultManager
    private lateinit var coordinator: PairingCoordinator
    private lateinit var dir: VaultDir
    private lateinit var recoveryKey: String
    private lateinit var scenario: ActivityScenario<MainActivity>

    companion object {
        const val APP = "todo"
        const val ORIGIN = "https://todo.example"
        const val DEVICE = "Stand-in Browser"

        private fun shell(cmd: String) {
            val fd = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(cmd)
            android.os.ParcelFileDescriptor.AutoCloseInputStream(fd).use { it.readBytes() }
        }

        /** The screen stays on and unlocked: screen off would lock every vault (by design). */
        @BeforeClass
        @JvmStatic
        fun awake() {
            shell("svc power stayon true")
            shell("input keyevent KEYCODE_WAKEUP")
            shell("wm dismiss-keyguard")
        }

        @AfterClass
        @JvmStatic
        fun relax() {
            shell("svc power stayon false")
        }
    }

    @Before
    fun setUp() {
        val port = InstrumentationRegistry.getArguments().getString("standIn")
            ?: throw AssertionError("no browser stand-in: run with -Pandroid.testInstrumentationRunnerArguments.standIn=<control port> (see android/ci/connected-test.sh)")
        standIn = StandIn(port.toInt())
        assertEquals("ok", standIn.ok("/healthz"))
        app.sweepOnce()
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        keys = VaultKeys(context, requireAuth = false)
        storage = VaultStorage(context, keys)
        manager = VaultManager(storage, keys)
        coordinator = PairingCoordinator(context, manager, storage) { LockSettings() }
        app.replacePairingForTest(coordinator)
        if (Build.VERSION.SDK_INT >= 33) {
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName, Manifest.permission.POST_NOTIFICATIONS)
        }
        val created = runBlocking { manager.create(identity, PassingPresence()).also { manager.confirmCreated(it) } }
        dir = created.dir
        recoveryKey = created.recoveryKey
        // In the foreground, as when the user scans (background FGS starts are refused).
        scenario = ActivityScenario.launch(MainActivity::class.java)
    }

    @After
    fun tearDown() {
        if (::standIn.isInitialized) runCatching { standIn.get("/lock", timeoutMs = 10_000) }
        if (::coordinator.isInitialized) {
            coordinator.stopAll()
            awaitTrue("service stopped after the test") { !SessionService.running.value }
        }
        if (::scenario.isInitialized) scenario.close()
        app.replacePairingForTest(null)
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        if (::keys.isInitialized) keys.vaultIdsWithKeys().forEach(keys::deleteKeys)
        if (::manager.isInitialized) manager.close()
    }

    private fun <T : Any> awaitValue(what: String, timeoutMs: Long = 30_000, value: () -> T?): T {
        val deadline = SystemClock.uptimeMillis() + timeoutMs
        while (true) {
            value()?.let { return it }
            if (SystemClock.uptimeMillis() > deadline) {
                val now = if (::coordinator.isInitialized) coordinator.pairing.value else null
                throw AssertionError("timed out waiting for $what (pairing: $now)")
            }
            Thread.sleep(50)
        }
    }

    private fun awaitTrue(what: String, cond: () -> Boolean) {
        awaitValue(what) { if (cond()) true else null }
    }

    private fun awaitState(what: String, pred: (State) -> Boolean): State =
        awaitValue(what) { coordinator.pairing.value?.state?.takeIf(pred) }

    private fun awaitEnded(): State.Ended = awaitState("the pairing's end") { it is State.Ended } as State.Ended

    /** The browser shows a code; the phone scans it. */
    private fun scanBrowserCode() {
        val qr = standIn.ok("/pair?app=${enc(APP)}&origin=${enc(ORIGIN)}&device=${enc(DEVICE)}")
        assertEquals(ScanResult.Started, runBlocking { coordinator.scan(qr) })
    }

    private fun toApproval(): ApprovalRequest {
        scanBrowserCode()
        return (awaitState("the approval prompt") { it is State.AwaitingApproval } as State.AwaitingApproval).request
    }

    private fun toRunning(presence: PassingPresence = PassingPresence()): State.Running {
        val request = toApproval()
        assertEquals("the codes match", standIn.ok("/sas"), request.sas)
        coordinator.approve(presence)
        standIn.ok("/confirm")
        assertEquals("active", standIn.ok("/state?until=active"))
        return awaitState("the running session") { it is State.Running } as State.Running
    }

    private fun sessionNotification(): Notification? =
        (context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).activeNotifications
            .firstOrNull { it.id == SessionService.NOTIFICATION_ID }?.notification

    private fun Notification.allText(): String =
        listOf(Notification.EXTRA_TITLE, Notification.EXTRA_TEXT, Notification.EXTRA_BIG_TEXT)
            .joinToString("\n") { extras.getCharSequence(it)?.toString().orEmpty() }

    private fun awaitServiceStopped() {
        awaitTrue("the service to stop") { !SessionService.running.value }
        awaitTrue("the notification to go") { sessionNotification() == null }
    }

    @Test
    fun fgs_starts_at_scan_and_stops_on_end() {
        assertFalse(SessionService.running.value)
        if (Build.VERSION.SDK_INT >= 34) {
            val info = context.packageManager.getServiceInfo(ComponentName(context, SessionService::class.java), 0)
            assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, info.foregroundServiceType)
        }
        scanBrowserCode()
        // Started at scan time, before anything is approved.
        awaitTrue("the service to start at scan") { SessionService.running.value }
        val request = (awaitState("the approval prompt") { it is State.AwaitingApproval } as State.AwaitingApproval).request
        val n = awaitValue("the session notification") { sessionNotification() }
        val text = n.allText()
        assertTrue(text, text.contains("$APP · $ORIGIN"))
        assertFalse("no code in the notification: $text", text.contains(request.sas))
        assertFalse("no device name in the notification: $text", text.contains(DEVICE))
        assertEquals(listOf(SessionService.STOP_LABEL), n.actions.map { it.title.toString() })

        // The pairing ends (denied): the vault stays locked and the service stops.
        coordinator.deny()
        assertEquals(State.Ended(identity, PairingOutcome.Denied(byUser = true)), awaitEnded())
        assertEquals("closed:denied", standIn.ok("/state?until=closed"))
        awaitServiceStopped()
        assertTrue(coordinator.running.value.isEmpty())
    }

    @Test
    fun approve_requires_crk_unwrap() {
        // The unlock prompt is cancelled: no CRK, so the answer is Deny.
        val cancelling = PassingPresence(pass = false)
        toApproval()
        coordinator.approve(cancelling)
        assertEquals(State.Ended(identity, PairingOutcome.Denied(byUser = true)), awaitEnded())
        assertEquals(1, cancelling.prompts)
        assertEquals("closed:denied", standIn.ok("/state?until=closed"))
        awaitServiceStopped()

        // K_auth can't unwrap (here: an unfinished re-wrap): no prompt at all,
        // Deny, and the app offers the re-wrap.
        assertTrue(dir.rewrapPending.createNewFile())
        val untouched = PassingPresence()
        toApproval()
        coordinator.approve(untouched)
        assertEquals(State.Ended(identity, PairingOutcome.KeyInvalidated), awaitEnded())
        assertEquals(0, untouched.prompts)
        assertEquals("closed:denied", standIn.ok("/state?until=closed"))
        assertTrue(dir.rewrapPending.delete())
        awaitServiceStopped()

        // Only the unwrapped CRK approves: the session runs.
        val passing = PassingPresence()
        val running = toRunning(passing)
        assertEquals(1, passing.prompts)
        assertEquals(State.Running(identity, running.sessionId, DEVICE), running)
    }

    @Test
    fun stop_action_locks() {
        toRunning()
        standIn.ok("/put?k=note&v=${enc("written before Stop")}")
        val n = awaitValue("the running notification") {
            sessionNotification()?.takeIf { it.extras.getCharSequence(Notification.EXTRA_TITLE)?.toString() == "Vault session running" }
        }
        // The notification's own Stop action, as the user taps it.
        n.actions.single { it.title.toString() == SessionService.STOP_LABEL }.actionIntent.send()

        assertEquals("closed:locked", standIn.ok("/state?until=closed"))
        assertEquals(State.Ended(identity, PairingOutcome.SessionEnded(EndReason.Locked)), awaitEnded())
        awaitServiceStopped()
        assertTrue(coordinator.running.value.isEmpty())
        // The browser lost its store with the session.
        assertEquals(409, standIn.get("/get?k=note").first)
    }

    @Test
    fun process_death_locks() {
        // Nothing about a running session may outlive the process: the system
        // never restarts the service, and the disk holds no secret in clear.
        assertEquals(Service.START_NOT_STICKY, SessionService.START_MODE)
        toRunning()
        val marker = "process-death-" + UUID.randomUUID()
        standIn.ok("/put?k=secret&v=$marker")
        assertEquals(marker, standIn.ok("/get?k=secret"))

        // The CRK, unwrapped here only to search the disk for it.
        val crk = runBlocking { manager.unwrapCrk(dir, PassingPresence()) }
        val death = File(context.cacheDir, "process-death")
        try {
            // A process death (the kill can't run inside the instrumented
            // process) leaves exactly what is on disk now.
            val files = context.dataDir.walkTopDown()
                .onEnter { !java.nio.file.Files.isSymbolicLink(it.toPath()) }
                .filter { it.isFile }
                .toList()
            val vaultRoot = dir.root.canonicalPath + File.separator
            assertTrue(files.any { it.canonicalPath.startsWith(vaultRoot) })
            val needles = mapOf(
                "the CRK" to crk,
                "vault data" to marker.toByteArray(),
                "the recovery key" to recoveryKey.toByteArray(),
            )
            for (f in files) {
                val bytes = f.readBytes()
                for ((what, needle) in needles) {
                    assertFalse("$what in clear in ${f.relativeTo(context.dataDir)}", bytes.contains(needle))
                }
            }

            // What a new process finds: the vault's files, which open locked.
            // (A copy, because this process still holds the container.)
            death.deleteRecursively()
            val copy = VaultDir(File(death, dir.id))
            assertTrue(dir.root.copyRecursively(copy.root))
            VaultManager(storage, keys).use { fresh ->
                runBlocking { fresh.open(copy) }.use { h ->
                    assertTrue(h.isLocked())
                    assertTrue(runBlocking { h.stats() }.locked)
                }
            }
        } finally {
            crk.fill(0)
            death.deleteRecursively()
        }
    }

    @Test
    fun e2e_pairing_with_browser_stand_in() {
        val request = toApproval()
        assertEquals(APP, request.appId)
        assertEquals(ORIGIN, request.origin)
        assertEquals(DEVICE, request.deviceName)
        assertTrue(request.relayHost, request.relayHost.startsWith("127.0.0.1:"))
        assertFalse(request.mailboxTaken)
        assertEquals("awaiting", standIn.ok("/state?until=awaiting"))
        // The user compares the codes, then approves with the unlock prompt.
        assertEquals(standIn.ok("/sas"), request.sas)
        val presence = PassingPresence()
        coordinator.approve(presence)
        standIn.ok("/confirm")
        assertEquals("active", standIn.ok("/state?until=active"))
        val running = awaitState("the running session") { it is State.Running } as State.Running
        assertEquals(DEVICE, running.deviceName)
        assertEquals(1, presence.prompts)
        assertEquals(listOf(identity), coordinator.running.value.map { it.identity })

        // The browser stores and reads through the vault.
        standIn.ok("/put?k=greeting&v=${enc("hello from the browser")}")
        assertEquals("hello from the browser", standIn.ok("/get?k=greeting"))

        // The browser ends the session: the vault locks, the service stops.
        assertTrue(standIn.ok("/lock").startsWith("closed"))
        assertTrue(awaitEnded().outcome is PairingOutcome.SessionEnded)
        awaitServiceStopped()

        // The next pairing needs a new unlock and finds the committed data.
        val again = PassingPresence()
        toRunning(again)
        assertEquals(1, again.prompts)
        assertEquals("hello from the browser", standIn.ok("/get?k=greeting"))
    }
}
