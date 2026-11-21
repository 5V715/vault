// SPDX-License-Identifier: MIT
package dev.vault.core

import dev.vault.core.PairingMachine.Effect
import dev.vault.core.PairingMachine.Event
import dev.vault.core.PairingMachine.State
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingMachineTest {
    private val identity = (VaultIdentity.of("todo", "https://todo.example") as Checked.Valid).value
    private val offer = Offer(appId = "todo", origin = "https://todo.example", relayHost = "relay.example")
    private val request = ApprovalRequest(
        appId = "todo",
        origin = "https://todo.example",
        relayHost = "relay.example",
        deviceName = "Firefox on laptop",
        sas = "123456",
        mailboxTaken = false,
    )

    /** Drives the machine and collects every effect in order. */
    private class Run(val m: PairingMachine = PairingMachine()) {
        val effects = mutableListOf<Effect>()

        fun on(e: Event): State {
            effects += m.on(e)
            return m.state
        }
    }

    private fun toPrompt(r: Run, req: ApprovalRequest = request) {
        r.on(Event.Scanned(offer, vault = identity))
        r.on(Event.ApprovalRequested(req))
    }

    @Test
    fun pairing_state_machine_happy_path() {
        val r = Run()
        assertEquals(State.Idle, r.m.state)
        assertEquals(State.Connecting(offer, identity), r.on(Event.Scanned(offer, vault = identity)))
        assertEquals(listOf(Effect.StartService(identity), Effect.ServeOffer(identity)), r.effects)
        r.effects.clear()

        assertEquals(State.AwaitingApproval(identity, request), r.on(Event.ApprovalRequested(request)))
        assertTrue((r.m.state as State.AwaitingApproval).canApprove)

        // "Codes match — approve" opens the BiometricPrompt; nothing is answered yet.
        assertEquals(State.Unlocking(identity, request), r.on(Event.UserApproved))
        assertEquals(listOf(Effect.PromptUserPresence(identity)), r.effects)
        r.effects.clear()

        // Only a successful CRK unwrap turns into an approval.
        assertEquals(State.Approving(identity), r.on(Event.CrkUnwrapped))
        assertEquals(listOf(Effect.AnswerApprove), r.effects)
        r.effects.clear()

        assertEquals(State.Running(identity, 7, "Firefox on laptop"), r.on(Event.SessionActive(7)))
        assertEquals(State.Ended(identity, PairingOutcome.SessionEnded(EndReason.Closed)), r.on(Event.SessionEnded(EndReason.Closed)))
        assertEquals(emptyList<Effect>(), r.effects)
        assertEquals("Session ended", (r.m.state as State.Ended).outcome.message)
    }

    @Test
    fun approve_requires_crk_unwrap() {
        // Approval without the CRK unwrap never answers Approve.
        for (fail in listOf(Event.UnwrapFailed, Event.UnwrapCancelled)) {
            val r = Run()
            toPrompt(r)
            r.on(Event.UserApproved)
            r.effects.clear()
            r.on(fail)
            assertEquals(listOf(Effect.AnswerDeny), r.effects)
            assertTrue(r.m.state is State.Ended)
        }
        // A stray unwrap result outside Unlocking is ignored.
        val r = Run()
        toPrompt(r)
        r.effects.clear()
        r.on(Event.CrkUnwrapped)
        assertEquals(emptyList<Effect>(), r.effects)
        assertEquals(State.AwaitingApproval(identity, request), r.m.state)
    }

    @Test
    fun pairing_denied_on_background() {
        // While the prompt is shown, and while the BiometricPrompt runs.
        for (approved in listOf(false, true)) {
            val r = Run()
            toPrompt(r)
            if (approved) r.on(Event.UserApproved)
            r.effects.clear()
            val s = r.on(Event.Backgrounded)
            assertEquals(listOf(Effect.AnswerDeny), r.effects)
            assertEquals(State.Ended(identity, PairingOutcome.Denied(byUser = false)), s)
            // A late unwrap after the deny must not approve.
            r.effects.clear()
            r.on(Event.CrkUnwrapped)
            assertEquals(emptyList<Effect>(), r.effects)
        }
        // An explicit deny.
        val r = Run()
        toPrompt(r)
        r.effects.clear()
        assertEquals(State.Ended(identity, PairingOutcome.Denied(byUser = true)), r.on(Event.UserDenied))
        assertEquals(listOf(Effect.AnswerDeny), r.effects)
        // Backgrounding a running session changes nothing: the service holds it.
        val running = Run()
        toPrompt(running)
        running.on(Event.UserApproved)
        running.on(Event.CrkUnwrapped)
        running.on(Event.SessionActive(3))
        running.effects.clear()
        running.on(Event.Backgrounded)
        assertTrue(running.m.state is State.Running)
        assertEquals(emptyList<Effect>(), running.effects)
    }

    @Test
    fun mailbox_taken_blocks_approval() {
        val taken = request.copy(deviceName = "", sas = "", mailboxTaken = true)
        val r = Run()
        toPrompt(r, taken)
        val s = r.m.state as State.AwaitingApproval
        assertFalse(s.canApprove)
        r.effects.clear()
        // "Approve" does nothing: no BiometricPrompt, no unwrap, no answer.
        r.on(Event.UserApproved)
        assertEquals(emptyList<Effect>(), r.effects)
        assertEquals(s, r.m.state)
        r.on(Event.CrkUnwrapped)
        assertEquals(emptyList<Effect>(), r.effects)
        // The only way out is a deny.
        assertEquals(State.Ended(identity, PairingOutcome.MailboxTaken), r.on(Event.UserDenied))
        assertEquals(listOf(Effect.AnswerDeny), r.effects)
        assertTrue(PairingOutcome.MailboxTaken.message.contains("someone else", ignoreCase = true))
    }

    @Test
    fun scan_errors_end_with_a_clear_message() {
        val noVault = Run()
        assertEquals(
            State.Ended(null, PairingOutcome.NoVaultForApp(offer)),
            noVault.on(Event.Scanned(offer, vault = null)),
        )
        assertEquals(emptyList<Effect>(), noVault.effects)
        assertEquals("No vault for this app", PairingOutcome.NoVaultForApp(offer).message)

        val bad = Run()
        assertEquals(State.Ended(null, PairingOutcome.BadCode(PairingCode.Scan.Malformed)), bad.on(Event.ScanRejected(PairingCode.Scan.Malformed)))

        // Connection problems before the prompt.
        for ((reason, outcome) in listOf(
            EndReason.Timeout to PairingOutcome.Expired,
            EndReason.Disconnected to PairingOutcome.RelayUnreachable,
            EndReason.WrongApp to PairingOutcome.WrongApp,
        )) {
            val r = Run()
            r.on(Event.Scanned(offer, vault = identity))
            assertEquals(State.Ended(identity, outcome), r.on(Event.SessionEnded(reason)))
            assertTrue(outcome.message.isNotBlank())
        }
        val down = Run()
        down.on(Event.Scanned(offer, vault = identity))
        assertEquals(State.Ended(identity, PairingOutcome.RelayUnreachable), down.on(Event.ConnectFailed))
    }

    @Test
    fun a_new_scan_needs_an_ended_machine() {
        val r = Run()
        toPrompt(r)
        r.effects.clear()
        r.on(Event.Scanned(offer, vault = identity))
        assertEquals(emptyList<Effect>(), r.effects)
        assertTrue(r.m.state is State.AwaitingApproval)
        r.on(Event.UserDenied)
        r.effects.clear()
        assertTrue(r.on(Event.Scanned(offer, vault = identity)) is State.Connecting)
        assertNull((r.effects.firstOrNull() as? Effect.AnswerDeny))
    }
}
