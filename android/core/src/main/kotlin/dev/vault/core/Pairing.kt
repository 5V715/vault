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

    /** The scanned text is not a vault code ([malformed]: it is one, but damaged). */
    data class BadCode(val malformed: Boolean) : PairingOutcome {
        override val message =
            if (malformed) PairingCode.Scan.Malformed.message else PairingCode.Scan.NotAVaultCode.message
    }

    /** No vault on this phone for the offer's app and origin: offer to create one. */
    data class NoVaultForApp(val offer: Offer) : PairingOutcome {
        override val message = "No vault for this app"
    }

    /** The vault's binding check refused [offer] (a different app or origin); offer to create one. */
    data class WrongApp(val offer: Offer) : PairingOutcome {
        override val message = "No vault for this app"
    }

    /** The user cancelled the scan, or left the app before the browser connected. */
    data object Cancelled : PairingOutcome {
        override val message = "Pairing cancelled"
    }

    /** K_auth was invalidated (the screen lock was removed): re-wrap first. */
    data object KeyInvalidated : PairingOutcome {
        override val message =
            "The vault's unlock key was invalidated because the screen lock was removed. Re-wrap it with the recovery key."
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
 * - Leaving the screen while approval is pending denies; leaving before the
 *   browser connects cancels.
 * - A CRK unwrapped anywhere but [State.Unlocking] yields [Effect.DiscardCrk].
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

        data class ScanRejected(val malformed: Boolean) : Event

        data object ConnectFailed : Event

        /**
         * `serve_offer` failed because the relay mailbox was already taken
         * (after the approver was warned, if the warning got through).
         */
        data object MailboxTaken : Event

        /** The user cancelled while the browser had not connected yet. */
        data object UserCancelled : Event

        data class ApprovalRequested(val request: ApprovalRequest) : Event

        /** "Codes match — approve". */
        data object UserApproved : Event

        data object UserDenied : Event

        /** The activity left the foreground or died. */
        data object Backgrounded : Event

        data object CrkUnwrapped : Event

        /** Authentication failed for good (lockout, no credential). */
        data object UnwrapFailed : Event

        data object UnwrapCancelled : Event

        /** K_auth was permanently invalidated: the vault needs a re-wrap. */
        data object UnwrapKeyInvalidated : Event

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

        /** Close the BiometricPrompt that is still showing. */
        data object CancelPrompt : Effect

        /** A CRK was unwrapped after the pairing moved on: wipe it, use it for nothing. */
        data object DiscardCrk : Effect

        /** Stop waiting for the browser (cancel `serveOffer`). */
        data object CancelServe : Effect
    }

    var state: State = State.Idle
        private set

    private var deviceName = ""

    /** Applies [event] and returns the effects to carry out, in order. */
    fun on(event: Event): List<Effect> {
        val s = state
        // A CRK is only ever used from Unlocking; anywhere else it is wiped.
        if (event == Event.CrkUnwrapped && s !is State.Unlocking) return listOf(Effect.DiscardCrk)
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
                state = State.Ended(null, PairingOutcome.BadCode(event.malformed))
                emptyList()
            }
            s is State.Connecting -> when (event) {
                is Event.ApprovalRequested -> {
                    state = State.AwaitingApproval(s.vault, event.request)
                    emptyList()
                }
                Event.ConnectFailed -> end(s.vault, PairingOutcome.RelayUnreachable)
                Event.MailboxTaken -> {
                    end(s.vault, PairingOutcome.MailboxTaken)
                    listOf(Effect.CancelServe)
                }
                // Leaving the app before the browser connects abandons the pairing,
                // so a request can't become approvable behind the user's back.
                Event.UserCancelled, Event.UserDenied, Event.Backgrounded -> {
                    end(s.vault, PairingOutcome.Cancelled)
                    listOf(Effect.CancelServe)
                }
                is Event.SessionEnded -> end(s.vault, beforePrompt(event.reason, s.offer))
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
                // Only a mailbox-taken warning can be on screen here (serve_offer
                // reports it after asking); answer it if it is still waiting.
                Event.MailboxTaken -> deny(s.vault, PairingOutcome.MailboxTaken)
                // Rust cancelled the prompt (timeout, browser gone): no answer needed.
                is Event.SessionEnded -> end(s.vault, beforePrompt(event.reason, null))
                else -> emptyList()
            }
            s is State.Unlocking -> when (event) {
                Event.CrkUnwrapped -> {
                    deviceName = s.request.deviceName
                    state = State.Approving(s.vault)
                    listOf(Effect.AnswerApprove)
                }
                // The prompt closed itself.
                Event.UnwrapCancelled -> deny(s.vault, PairingOutcome.Denied(byUser = true))
                Event.UnwrapFailed -> deny(s.vault, PairingOutcome.UnlockFailed)
                Event.UnwrapKeyInvalidated -> deny(s.vault, PairingOutcome.KeyInvalidated)
                // The prompt is still up: close it.
                Event.UserDenied -> listOf(Effect.CancelPrompt) + deny(s.vault, PairingOutcome.Denied(byUser = true))
                Event.Backgrounded -> listOf(Effect.CancelPrompt) + deny(s.vault, PairingOutcome.Denied(byUser = false))
                is Event.SessionEnded -> {
                    end(s.vault, beforePrompt(event.reason, null))
                    listOf(Effect.CancelPrompt)
                }
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

    private fun beforePrompt(reason: EndReason, offer: Offer?): PairingOutcome = when (reason) {
        EndReason.Timeout, EndReason.ApprovalTimeout -> PairingOutcome.Expired
        EndReason.Disconnected -> PairingOutcome.RelayUnreachable
        EndReason.WrongApp -> if (offer != null) PairingOutcome.WrongApp(offer) else PairingOutcome.SessionEnded(reason)
        EndReason.Busy -> PairingOutcome.Busy
        EndReason.UnlockFailed -> PairingOutcome.UnlockFailed
        else -> PairingOutcome.SessionEnded(reason)
    }
}
