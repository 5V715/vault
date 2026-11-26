// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.os.Build
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import dev.vault.android.MainActivity
import dev.vault.android.VaultApp
import dev.vault.android.keys.VaultKeys
import dev.vault.android.replacePairingForTest
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.ApprovalRequest
import dev.vault.core.Checked
import dev.vault.core.LockSettings
import dev.vault.core.PairingMachine.State
import dev.vault.core.VaultIdentity
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.net.URLEncoder
import javax.crypto.Cipher
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals

const val APP = "todo"
const val ORIGIN = "https://todo.example"
const val DEVICE = "Stand-in Browser"

/**
 * The test stand-in for the user at the BiometricPrompt: test keys
 * (`VaultKeys(requireAuth = false)`) need no authentication, so it passes the
 * CryptoObject's cipher back unchanged, or cancels ([pass] = false). It exists
 * only in androidTest; the app's approver is the real one.
 */
class PassingPresence(private val pass: Boolean = true) : UserPresence {
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
class StandIn(private val port: Int) {
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

fun enc(s: String): String = URLEncoder.encode(s, "UTF-8")

fun ByteArray.containsBytes(needle: ByteArray): Boolean {
    if (needle.isEmpty() || needle.size > size) return false
    outer@ for (i in 0..size - needle.size) {
        for (j in needle.indices) if (this[i + j] != needle[j]) continue@outer
        return true
    }
    return false
}

/** Runs [cmd] as the shell user and returns its output. */
fun shell(cmd: String): String {
    val fd = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(cmd)
    return ParcelFileDescriptor.AutoCloseInputStream(fd).use { it.readBytes().toString(Charsets.UTF_8) }
}

/** The screen stays on and unlocked: screen off would lock every vault (by design). */
fun keepScreenOn() {
    shell("svc power stayon true")
    shell("input keyevent KEYCODE_WAKEUP")
    shell("wm dismiss-keyguard")
}

/**
 * One vault for [APP] at [ORIGIN] over test keys, the coordinator the real
 * [SessionService] uses (through the debug-only hook), the stand-in and
 * MainActivity in the foreground, plus the waits the pairing tests share.
 */
class PairingHarness {
    val instrumentation = InstrumentationRegistry.getInstrumentation()
    val context: Context = instrumentation.targetContext
    val app = context.applicationContext as VaultApp
    val identity = (VaultIdentity.of(APP, ORIGIN) as Checked.Valid).value
    lateinit var standIn: StandIn
    lateinit var keys: VaultKeys
    lateinit var storage: VaultStorage
    lateinit var manager: VaultManager
    lateinit var coordinator: PairingCoordinator
    lateinit var dir: VaultDir
    var recoveryKey: String? = null
    private var scenario: ActivityScenario<MainActivity>? = null

    /** [fresh]: a new vault; otherwise the one an earlier phase (process) left. */
    fun open(fresh: Boolean) {
        val port = InstrumentationRegistry.getArguments().getString("standIn")
            ?: throw AssertionError("no browser stand-in: run with -Pandroid.testInstrumentationRunnerArguments.standIn=<control port> (see android/ci/connected-test.sh)")
        standIn = StandIn(port.toInt())
        assertEquals("ok", standIn.ok("/healthz"))
        app.sweepOnce()
        if (fresh) File(context.noBackupFilesDir, "vaults").deleteRecursively()
        keys = VaultKeys(context, requireAuth = false)
        storage = VaultStorage(context, keys)
        manager = VaultManager(storage, keys)
        coordinator = PairingCoordinator(context, manager, storage) { LockSettings() }
        app.replacePairingForTest(coordinator)
        if (Build.VERSION.SDK_INT >= 33) {
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName, Manifest.permission.POST_NOTIFICATIONS)
        }
        if (fresh) {
            val created = runBlocking { manager.create(identity, PassingPresence()).also { manager.confirmCreated(it) } }
            dir = created.dir
            recoveryKey = created.recoveryKey
        } else {
            dir = storage.dirOf(identity) ?: throw AssertionError("the earlier phase's vault is gone")
        }
        // In the foreground, as when the user scans (background FGS starts are refused).
        scenario = ActivityScenario.launch(MainActivity::class.java)
    }

    val activity: ActivityScenario<MainActivity> get() = checkNotNull(scenario)

    fun close() {
        if (::standIn.isInitialized) runCatching { standIn.get("/lock", timeoutMs = 10_000) }
        if (::coordinator.isInitialized) {
            coordinator.stopAll()
            awaitTrue("service stopped after the test") { !SessionService.running.value }
        }
        scenario?.close()
        app.replacePairingForTest(null)
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        if (::keys.isInitialized) keys.vaultIdsWithKeys().forEach(keys::deleteKeys)
        if (::manager.isInitialized) manager.close()
    }

    fun <T : Any> awaitValue(what: String, timeoutMs: Long = 30_000, value: () -> T?): T {
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

    fun awaitTrue(what: String, cond: () -> Boolean) {
        awaitValue(what) { if (cond()) true else null }
    }

    fun awaitState(what: String, pred: (State) -> Boolean): State =
        awaitValue(what) { coordinator.pairing.value?.state?.takeIf(pred) }

    fun awaitEnded(): State.Ended = awaitState("the pairing's end") { it is State.Ended } as State.Ended

    /** The browser shows a code; the phone scans it. */
    fun scanBrowserCode() {
        val qr = standIn.ok("/pair?app=${enc(APP)}&origin=${enc(ORIGIN)}&device=${enc(DEVICE)}")
        assertEquals(ScanResult.Started, runBlocking { coordinator.scan(qr) })
    }

    fun toApproval(): ApprovalRequest {
        scanBrowserCode()
        return (awaitState("the approval prompt") { it is State.AwaitingApproval } as State.AwaitingApproval).request
    }

    fun toRunning(presence: PassingPresence = PassingPresence()): State.Running {
        val request = toApproval()
        assertEquals("the codes match", standIn.ok("/sas"), request.sas)
        coordinator.approve(presence)
        standIn.ok("/confirm")
        assertEquals("active", standIn.ok("/state?until=active"))
        return awaitState("the running session") { it is State.Running } as State.Running
    }

    fun sessionNotification(): Notification? =
        (context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).activeNotifications
            .firstOrNull { it.id == SessionService.NOTIFICATION_ID }?.notification

    fun awaitServiceStopped() {
        awaitTrue("the service to stop") { !SessionService.running.value }
        awaitTrue("the notification to go") { sessionNotification() == null }
    }
}

fun Notification.allText(): String =
    listOf(Notification.EXTRA_TITLE, Notification.EXTRA_TEXT, Notification.EXTRA_BIG_TEXT)
        .joinToString("\n") { extras.getCharSequence(it)?.toString().orEmpty() }
