// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.app.ActivityManager
import android.app.Notification
import android.content.Context
import android.content.ComponentName
import android.content.pm.ServiceInfo
import android.os.Build
import androidx.lifecycle.Lifecycle
import androidx.test.ext.junit.runners.AndroidJUnit4
import dev.vault.core.EndReason
import dev.vault.core.PairingMachine.State
import dev.vault.core.PairingOutcome
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Pairing against a real relay and a browser stand-in (vault-client, native):
 * the scan, the foreground service, the approval with the CRK unwrap, the
 * session and Stop. Process death is [ProcessDeathTest] (host-driven).
 *
 * Needs the stand-in: CI starts it on the host and passes its control port as
 * the instrumentation argument `standIn` (adb reverse makes 127.0.0.1 reach
 * the host). Emulators can't pass a BiometricPrompt, so the vault uses test
 * keys and [PassingPresence] stands in for the user at the prompt.
 */
@RunWith(AndroidJUnit4::class)
class PairingTest {
    private val h = PairingHarness()

    companion object {
        @BeforeClass
        @JvmStatic
        fun awake() = keepScreenOn()
    }

    @Before
    fun setUp() = h.open(fresh = true)

    @After
    fun tearDown() = h.close()

    @Test
    fun fgs_starts_at_scan_and_stops_on_end() {
        with(h) {
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
            // In the foreground, as the system sees it: its notification is the
            // service's, and (34+) the live service has the specialUse type.
            assertTrue("FLAG_FOREGROUND_SERVICE", (n.flags and Notification.FLAG_FOREGROUND_SERVICE) != 0)
            val type = awaitValue("the live service") { SessionService.liveForegroundServiceType() }
            if (Build.VERSION.SDK_INT >= 34) assertEquals(ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE, type)
            @Suppress("DEPRECATION") // still lists the caller's own services
            val mine = (context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager)
                .getRunningServices(Int.MAX_VALUE)
                .single { it.service.className == SessionService::class.java.name }
            assertTrue("the system runs it as a foreground service", mine.foreground)
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
    }

    @Test
    fun approve_requires_crk_unwrap() {
        with(h) {
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
    }

    @Test
    fun stop_action_locks() {
        with(h) {
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
    }

    @Test
    fun e2e_pairing_with_browser_stand_in() {
        with(h) {
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

    @Test
    fun leaving_the_app_denies_a_pending_approval() {
        with(h) {
            toApproval()
            // MainActivity.onStop: the app left the foreground with the prompt up.
            activity.moveToState(Lifecycle.State.CREATED)
            assertEquals(State.Ended(identity, PairingOutcome.Denied(byUser = false)), awaitEnded())
            assertEquals("closed:denied", standIn.ok("/state?until=closed"))
            awaitServiceStopped()
            activity.moveToState(Lifecycle.State.RESUMED)
        }
    }

    @Test
    fun mailbox_taken_blocks_approval_without_touching_the_keystore() {
        with(h) {
            val qr = standIn.ok("/pair?app=${enc(APP)}&origin=${enc(ORIGIN)}&device=${enc(DEVICE)}")
            // Someone else answers the code first: the relay refuses the phone (409).
            standIn.ok("/occupy")
            assertEquals(ScanResult.Started, runBlocking { coordinator.scan(qr) })
            val shown = awaitState("the mailbox-taken warning") { it is State.AwaitingApproval } as State.AwaitingApproval
            assertTrue(shown.request.mailboxTaken)
            assertFalse(shown.canApprove)
            // "Approve" does nothing: no prompt, no Keystore use, still the warning.
            val presence = PassingPresence()
            coordinator.approve(presence)
            Thread.sleep(500)
            assertEquals(0, presence.prompts)
            assertTrue(coordinator.pairing.value?.state is State.AwaitingApproval)
            coordinator.deny()
            assertEquals(State.Ended(identity, PairingOutcome.MailboxTaken), awaitEnded())
            assertEquals(0, presence.prompts)
            awaitServiceStopped()
        }
    }
}
