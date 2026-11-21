// SPDX-License-Identifier: MIT
package dev.vault.core

/** What a scanned pairing code asks for (vault-ffi `OfferInfo`). Untrusted. */
data class Offer(val appId: String, val origin: String, val relayHost: String)

/**
 * What the approval screen shows (vault-ffi `ApprovalRequestFfi`). Everything
 * but [sas] and [mailboxTaken] comes from the browser: display it as untrusted.
 */
data class ApprovalRequest(
    val appId: String,
    val origin: String,
    val relayHost: String,
    val deviceName: String,
    val sas: String,
    val mailboxTaken: Boolean,
)

/** How a session ended (vault-ffi `EndReasonFfi`, one to one). */
enum class EndReason {
    Closed,
    Disconnected,
    Denied,
    ApprovalTimeout,
    NotApproved,
    Replaced,
    Locked,
    Timeout,
    IdleTimeout,
    ProtocolError,
    Busy,
    Internal,
    WrongApp,
    UnlockFailed,
}

/** How a pairing attempt ended, with the message the app shows. */
sealed interface PairingOutcome {
    val message: String

    data class BadCode(val scan: PairingCode.Scan) : PairingOutcome {
        override val message = when (scan) {
            PairingCode.Scan.Malformed -> PairingCode.Scan.Malformed.message
            else -> PairingCode.Scan.NotAVaultCode.message
        }
    }

    /** No vault on this phone for the offer's app and origin: offer to create one. */
    data class NoVaultForApp(val offer: Offer) : PairingOutcome {
        override val message = "No vault for this app"
    }

    /** The vault's binding check refused the offer (a different app or origin). */
    data object WrongApp : PairingOutcome {
        override val message = "No vault for this app"
    }

    data object Expired : PairingOutcome {
        override val message = "The pairing code expired. Show a new one in the browser."
    }

    data object RelayUnreachable : PairingOutcome {
        override val message = "Cannot reach the relay. Check the connection and try again."
    }

    data object Busy : PairingOutcome {
        override val message = "This vault already serves a browser. Stop that session first."
    }

    /** The relay mailbox was already taken: someone may be attacking the pairing. */
    data object MailboxTaken : PairingOutcome {
        override val message =
            "Someone else answered this pairing code first. Do not approve; show a new code in the browser."
    }

    data class Denied(val byUser: Boolean) : PairingOutcome {
        override val message = if (byUser) "Denied" else "Denied: the app left the screen during approval"
    }

    data object UnlockFailed : PairingOutcome {
        override val message = "The vault could not be unlocked. Nothing was shared."
    }

    data class SessionEnded(val reason: EndReason) : PairingOutcome {
        override val message = when (reason) {
            EndReason.IdleTimeout -> "Session ended: idle"
            EndReason.Locked -> "Session ended: vault locked"
            else -> "Session ended"
        }
    }
}

/**
 * One pairing, from scan to session end. Pure: the app feeds it [Event]s and
 * carries out the returned [Effect]s (start the service, serve the offer, show
 * the BiometricPrompt, answer the approver).
 *
 * Invariants:
 * - [Effect.AnswerApprove] comes only from [State.Unlocking] after
 *   [Event.CrkUnwrapped]: approving needs user presence and the CRK unwrap.
 * - A mailbox-taken request can only be denied.
 * - Leaving the screen while approval is pending denies.
 */
class PairingMachine {
    sealed interface State {
        data object Idle : State

        /** The service runs and vault-server waits for the browser's handshake. */
        data class Connecting(val offer: Offer, val vault: VaultIdentity) : State

        /** The approval screen is shown. */
        data class AwaitingApproval(val vault: VaultIdentity, val request: ApprovalRequest) : State {
            val canApprove: Boolean get() = !request.mailboxTaken
        }

        /** The BiometricPrompt runs to unwrap the CRK. */
        data class Unlocking(val vault: VaultIdentity, val request: ApprovalRequest) : State

        /** Approve was answered; vault-server unlocks and confirms. */
        data class Approving(val vault: VaultIdentity) : State

        data class Running(val vault: VaultIdentity, val sessionId: Long, val deviceName: String) : State

        data class Ended(val vault: VaultIdentity?, val outcome: PairingOutcome) : State
    }

    sealed interface Event {
        /** A pairing code was parsed; [vault] is the matching vault on the phone, if any. */
        data class Scanned(val offer: Offer, val vault: VaultIdentity?) : Event

        data class ScanRejected(val scan: PairingCode.Scan) : Event

        data object ConnectFailed : Event

        data class ApprovalRequested(val request: ApprovalRequest) : Event

        /** "Codes match — approve". */
        data object UserApproved : Event

        data object UserDenied : Event

        /** The activity left the foreground or died. */
        data object Backgrounded : Event

        data object CrkUnwrapped : Event

        data object UnwrapFailed : Event

        data object UnwrapCancelled : Event

        data class SessionActive(val sessionId: Long) : Event

        data class SessionEnded(val reason: EndReason) : Event
    }

    sealed interface Effect {
        data class StartService(val vault: VaultIdentity) : Effect

        data class ServeOffer(val vault: VaultIdentity) : Effect

        /** Show the BiometricPrompt with a CryptoObject on the vault's K_auth. */
        data class PromptUserPresence(val vault: VaultIdentity) : Effect

        /** Answer the approver with the just-unwrapped CRK (then wipe it). */
        data object AnswerApprove : Effect

        data object AnswerDeny : Effect
    }

    var state: State = State.Idle
        private set

    private var deviceName = ""

    /** Applies [event] and returns the effects to carry out, in order. */
    fun on(event: Event): List<Effect> {
        val s = state
        return when {
            event is Event.Scanned && (s is State.Idle || s is State.Ended) -> {
                val v = event.vault
                if (v == null) {
                    state = State.Ended(null, PairingOutcome.NoVaultForApp(event.offer))
                    emptyList()
                } else {
                    state = State.Connecting(event.offer, v)
                    listOf(Effect.StartService(v), Effect.ServeOffer(v))
                }
            }
            event is Event.ScanRejected && (s is State.Idle || s is State.Ended) -> {
                state = State.Ended(null, PairingOutcome.BadCode(event.scan))
                emptyList()
            }
            s is State.Connecting -> when (event) {
                is Event.ApprovalRequested -> {
                    state = State.AwaitingApproval(s.vault, event.request)
                    emptyList()
                }
                Event.ConnectFailed -> end(s.vault, PairingOutcome.RelayUnreachable)
                is Event.SessionEnded -> end(s.vault, beforePrompt(event.reason))
                else -> emptyList()
            }
            s is State.AwaitingApproval -> when (event) {
                Event.UserApproved -> if (s.canApprove) {
                    state = State.Unlocking(s.vault, s.request)
                    listOf(Effect.PromptUserPresence(s.vault))
                } else {
                    emptyList()
                }
                Event.UserDenied -> deny(
                    s.vault,
                    if (s.request.mailboxTaken) PairingOutcome.MailboxTaken else PairingOutcome.Denied(byUser = true),
                )
                Event.Backgrounded -> deny(
                    s.vault,
                    if (s.request.mailboxTaken) PairingOutcome.MailboxTaken else PairingOutcome.Denied(byUser = false),
                )
                // Rust cancelled the prompt (timeout, browser gone): no answer needed.
                is Event.SessionEnded -> end(s.vault, beforePrompt(event.reason))
                else -> emptyList()
            }
            s is State.Unlocking -> when (event) {
                Event.CrkUnwrapped -> {
                    deviceName = s.request.deviceName
                    state = State.Approving(s.vault)
                    listOf(Effect.AnswerApprove)
                }
                Event.UnwrapFailed, Event.UnwrapCancelled, Event.UserDenied ->
                    deny(s.vault, PairingOutcome.Denied(byUser = true))
                Event.Backgrounded -> deny(s.vault, PairingOutcome.Denied(byUser = false))
                is Event.SessionEnded -> end(s.vault, beforePrompt(event.reason))
                else -> emptyList()
            }
            s is State.Approving -> when (event) {
                is Event.SessionActive -> {
                    state = State.Running(s.vault, event.sessionId, deviceName)
                    emptyList()
                }
                is Event.SessionEnded -> end(
                    s.vault,
                    if (event.reason == EndReason.UnlockFailed) PairingOutcome.UnlockFailed else PairingOutcome.SessionEnded(event.reason),
                )
                else -> emptyList()
            }
            s is State.Running -> when (event) {
                is Event.SessionEnded -> end(s.vault, PairingOutcome.SessionEnded(event.reason))
                else -> emptyList()
            }
            else -> emptyList()
        }
    }

    private fun end(vault: VaultIdentity?, outcome: PairingOutcome): List<Effect> {
        state = State.Ended(vault, outcome)
        deviceName = ""
        return emptyList()
    }

    private fun deny(vault: VaultIdentity, outcome: PairingOutcome): List<Effect> {
        end(vault, outcome)
        return listOf(Effect.AnswerDeny)
    }

    private fun beforePrompt(reason: EndReason): PairingOutcome = when (reason) {
        EndReason.Timeout, EndReason.ApprovalTimeout -> PairingOutcome.Expired
        EndReason.Disconnected -> PairingOutcome.RelayUnreachable
        EndReason.WrongApp -> PairingOutcome.WrongApp
        EndReason.Busy -> PairingOutcome.Busy
        EndReason.UnlockFailed -> PairingOutcome.UnlockFailed
        else -> PairingOutcome.SessionEnded(reason)
    }
}
