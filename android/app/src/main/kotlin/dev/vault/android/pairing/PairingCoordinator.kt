// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.content.Context
import android.content.Intent
import dev.vault.android.keys.AuthKeyInvalidatedException
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.PresenceCancelledException
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.ApprovalRequest
import dev.vault.core.EndReason
import dev.vault.core.LockSettings
import dev.vault.core.Offer
import dev.vault.core.PairingCode
import dev.vault.core.PairingMachine
import dev.vault.core.PairingMachine.Effect
import dev.vault.core.PairingMachine.Event
import dev.vault.core.PairingMachine.State
import dev.vault.core.PairingOutcome
import dev.vault.core.SessionNotice
import dev.vault.core.SessionSupervisor
import dev.vault.core.VaultCatalog
import dev.vault.core.VaultIdentity
import kotlin.coroutines.cancellation.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import uniffi.vault_ffi.ApprovalFfi
import uniffi.vault_ffi.ApprovalRequestFfi
import uniffi.vault_ffi.ApproverCallback
import uniffi.vault_ffi.EndReasonFfi
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.SessionListener
import uniffi.vault_ffi.SessionStateFfi
import uniffi.vault_ffi.VaultHandle

/** The pairing the screens show: its vault (null when no vault matched) and machine state. */
data class PairingView(val vaultId: String?, val state: State)

/** A vault serving a browser. [deviceName] is browser-supplied: display it as untrusted. */
data class RunningSession(val vaultId: String, val identity: VaultIdentity, val deviceName: String, val sinceMillis: Long)

/** What a scanned text led to. */
sealed interface ScanResult {
    /** Not a usable pairing code: show [message] and keep scanning. */
    data class Rejected(val message: String) : ScanResult

    /** A pairing code: [PairingCoordinator.pairing] shows how it goes (including "no vault"). */
    data object Started : ScanResult
}

/** Wipes the CRK an approval carries. */
internal fun ApprovalFfi.wipe() {
    if (this is ApprovalFfi.Approve) crk.fill(0)
}

/**
 * One approval prompt's answer, handed from the screen to vault-ffi's
 * approver call. An answer given after the call was abandoned (Rust cancelled
 * it), or a second answer, is wiped at once.
 */
internal class ApprovalSlot {
    private val result = CompletableDeferred<ApprovalFfi>()
    private var given: ApprovalFfi? = null
    private var abandoned = false

    /** Delivers [answer]; false (and [answer] wiped) if the prompt is gone or already answered. */
    @Synchronized
    fun answer(answer: ApprovalFfi): Boolean {
        if (abandoned || given != null) {
            answer.wipe()
            return false
        }
        given = answer
        result.complete(answer)
        return true
    }

    suspend fun await(): ApprovalFfi = result.await()

    /** The approver call ended without taking the answer: wipe what was given. */
    @Synchronized
    fun abandon() {
        abandoned = true
        given?.wipe()
        result.cancel()
    }
}

/** How long a denied pairing's session may take to end by itself before its vault is closed. */
private const val ANSWER_GRACE_MS = 5_000L

internal fun EndReasonFfi.toCore(): EndReason = when (this) {
    EndReasonFfi.CLOSED -> EndReason.Closed
    EndReasonFfi.DISCONNECTED -> EndReason.Disconnected
    EndReasonFfi.DENIED -> EndReason.Denied
    EndReasonFfi.APPROVAL_TIMEOUT -> EndReason.ApprovalTimeout
    EndReasonFfi.NOT_APPROVED -> EndReason.NotApproved
    EndReasonFfi.REPLACED -> EndReason.Replaced
    EndReasonFfi.LOCKED -> EndReason.Locked
    EndReasonFfi.TIMEOUT -> EndReason.Timeout
    EndReasonFfi.IDLE_TIMEOUT -> EndReason.IdleTimeout
    EndReasonFfi.PROTOCOL_ERROR -> EndReason.ProtocolError
    EndReasonFfi.BUSY -> EndReason.Busy
    EndReasonFfi.INTERNAL -> EndReason.Internal
    EndReasonFfi.WRONG_APP -> EndReason.WrongApp
    EndReasonFfi.UNLOCK_FAILED -> EndReason.UnlockFailed
}

internal fun ApprovalRequestFfi.toCore() = ApprovalRequest(
    appId = appId,
    origin = origin,
    relayHost = relayHost,
    deviceName = deviceName,
    sas = sas,
    mailboxTaken = mailboxTaken,
)

/**
 * Runs pairings and sessions: the [PairingMachine] of each vault decides,
 * [SessionSupervisor] decides when vaults lock and the foreground service
 * stops, and this class carries out their effects with vault-ffi, the
 * Keystore (through [VaultManager]) and [SessionService].
 *
 * Every vault handle that pairs or serves is opened here only after the
 * foreground service was asked to start, and every one is locked and closed
 * when its pairing or session ends, on Stop, on screen off (by default) and
 * when the service is destroyed. Nothing about a session is persisted: process
 * death leaves every vault locked.
 *
 * All state is confined to the main thread; the public methods may be called
 * from any thread.
 */
class PairingCoordinator(
    context: Context,
    private val manager: VaultManager,
    private val storage: VaultStorage,
    lockSettings: () -> LockSettings,
) {
    private val context = context.applicationContext
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val supervisor = SessionSupervisor(lockSettings)
    private val catalog = VaultCatalog(storage)

    /** A vault the service holds while it pairs or serves. */
    private inner class Entry(val dir: VaultDir, val identity: VaultIdentity, val qr: String) {
        val id: String get() = dir.id
        val machine = PairingMachine()
        var handle: VaultHandle? = null
        var serveJob: Job? = null
        var unwrapJob: Job? = null

        /** The approver call waiting for the user's answer. */
        var slot: ApprovalSlot? = null

        /** The CRK between its unwrap and the approval (wiped on any other path). */
        var crk: ByteArray? = null

        /** The approval handed to vault-ffi; wiped once Rust reported back. */
        var handed: ApprovalFfi? = null

        /** The screen's BiometricPrompt for this pairing. */
        var presence: UserPresence? = null
        var since = 0L
        var closed = false

        /** The prompt was answered (approve or deny): vault-server ends or runs the session. */
        var answered = false

        /** vault-server reported the session's end (or serve_offer failed: there is none). */
        val sessionOver = CompletableDeferred<Unit>()
    }

    private val entries = LinkedHashMap<String, Entry>()

    /** Vault id → the job locking and closing a removed entry's handle. */
    private val closing = HashMap<String, Job>()
    /**
     * The pairing on screen, by object: a vault's ended entry (still closing)
     * and a new pairing of the same vault share its id but not this.
     */
    private var focused: Entry? = null
    private var service: SessionService? = null

    private val queue = ArrayDeque<Pair<Entry, Event>>()
    private var draining = false

    private val pairingState = MutableStateFlow<PairingView?>(null)
    private val runningState = MutableStateFlow<List<RunningSession>>(emptyList())

    /** The pairing on screen (the latest scan), or null. */
    val pairing: StateFlow<PairingView?> = pairingState.asStateFlow()

    /** The vaults serving a browser now. */
    val running: StateFlow<List<RunningSession>> = runningState.asStateFlow()

    /**
     * A scanned [text]: only `vault-pair:v1:` codes start a pairing. A code
     * for an app and origin without a vault ends at once with "No vault for
     * this app"; nothing is dialed then and the service does not start.
     */
    suspend fun scan(text: String): ScanResult = withContext(Dispatchers.Main.immediate) {
        val code = PairingCode.classify(text)
        if (code !is PairingCode.Scan.Offer) return@withContext rejected(code is PairingCode.Scan.Malformed)
        val info = try {
            manager.factory.parseOffer(code.payload)
        } catch (e: FfiException) {
            return@withContext rejected(malformed = true)
        }
        val offer = Offer(info.appId, info.origin, info.relayHost)
        val found = withContext(Dispatchers.IO) {
            catalog.vaultFor(offer)?.let { id -> storage.dirOf(id)?.let { dir -> dir to id } }
        }
        if (found == null) {
            val m = PairingMachine()
            m.on(Event.Scanned(offer, vault = null))
            focused = null
            pairingState.value = PairingView(null, m.state)
            return@withContext ScanResult.Started
        }
        val (dir, identity) = found
        if (entries.containsKey(dir.id)) {
            // One session per vault: this one already pairs or serves a browser.
            focused = null
            pairingState.value = PairingView(dir.id, State.Ended(identity, PairingOutcome.Busy))
            return@withContext ScanResult.Started
        }
        val e = Entry(dir, identity, code.payload)
        entries[e.id] = e
        focused = e
        post(e, Event.Scanned(offer, vault = identity))
        ScanResult.Started
    }

    /** "Codes match — approve": the BiometricPrompt ([presence]) unwraps the CRK first. */
    fun approve(presence: UserPresence) {
        scope.launch {
            val e = focused ?: return@launch
            e.presence = presence
            post(e, Event.UserApproved)
        }
    }

    /** "Deny", or cancel while waiting for the browser. */
    fun deny() {
        scope.launch { focused?.let { post(it, Event.UserDenied) } }
    }

    /** The app's screen left the foreground: pending approvals are denied. */
    fun backgrounded() {
        scope.launch { entries.values.toList().forEach { post(it, Event.Backgrounded) } }
    }

    /** Ends one vault's session from the app (it locks; the service stops when idle). */
    fun stopSession(vaultId: String) {
        scope.launch {
            val e = entries[vaultId] ?: return@launch
            if (e.machine.state is State.Running) {
                e.handle?.lock()
            } else {
                post(e, Event.UserDenied)
            }
        }
    }

    /** The notification's Stop: locks every vault and stops the service. */
    fun stopAll() {
        scope.launch { perform(supervisor.onStop(), forced = true) }
    }

    /** The screen turned off: lock everything (setting) or at least deny pending approvals. */
    fun onScreenOff() {
        scope.launch { perform(supervisor.onScreenOff(), forced = true) }
    }

    /** Clears an ended pairing from the screen. */
    fun dismiss() {
        scope.launch {
            if (pairingState.value?.state is State.Ended) {
                pairingState.value = null
                focused = null
            }
        }
    }

    /** The notification text (main thread). */
    internal fun notice(): SessionNotice {
        val (serving, pairing) = entries.values.partition { it.machine.state is State.Running }
        return SessionNotice.of(serving.map { it.identity }, pairing.map { it.identity })
    }

    /** No vault pairs or serves (main thread). */
    internal val isIdle: Boolean get() = supervisor.isIdle

    internal fun attach(s: SessionService) {
        service = s
    }

    /**
     * The service is going away (main thread). Stopped by the system rather
     * than because nothing was left ([requested]): nothing may stay unlocked
     * without it. After a requested stop, a pairing scanned since then has
     * already started a new service instance and is left alone.
     */
    internal fun detach(s: SessionService, requested: Boolean) {
        if (service === s) service = null
        if (!requested) perform(supervisor.onStop(), forced = true)
    }

    private fun rejected(malformed: Boolean): ScanResult.Rejected {
        val m = PairingMachine()
        m.on(Event.ScanRejected(malformed))
        return ScanResult.Rejected((m.state as State.Ended).outcome.message)
    }

    /** Feeds [event] to [e]'s machine, in order, never re-entrantly (main thread). */
    private fun post(e: Entry, event: Event) {
        queue.addLast(e to event)
        if (draining) return
        draining = true
        try {
            while (true) {
                val (entry, ev) = queue.removeFirstOrNull() ?: break
                handle(entry, ev)
            }
        } finally {
            draining = false
        }
    }

    private fun handle(e: Entry, event: Event) {
        val before = e.machine.state
        val effects = e.machine.on(event)
        val after = e.machine.state
        for (f in effects) effect(e, f)
        if (event is Event.ApprovalRequested && after !is State.AwaitingApproval) {
            // Not shown (cancelled, ended): the prompt is denied.
            e.slot?.answer(ApprovalFfi.Deny)
            e.slot = null
        }
        if (event is Event.SessionActive || event is Event.SessionEnded) {
            // Rust has taken the approval: the handed CRK can go.
            e.handed?.wipe()
            e.handed = null
        }
        if (after is State.Running && before !is State.Running) {
            e.since = System.currentTimeMillis()
            perform(supervisor.onSessionActive(e.id, after.sessionId))
        }
        if (after is State.Ended && before !is State.Ended) {
            e.presence = null
            perform(
                if (before is State.Running) {
                    supervisor.onSessionEnded(e.id, before.sessionId)
                } else {
                    supervisor.onPairingFinished(e.id)
                },
            )
        }
        if (e === focused) pairingState.value = PairingView(e.id, after)
        publish()
    }

    private fun effect(e: Entry, f: Effect) {
        when (f) {
            is Effect.StartService -> startService(e)
            is Effect.ServeOffer -> e.serveJob = scope.launch { serve(e) }
            is Effect.PromptUserPresence -> {
                val p = e.presence
                e.unwrapJob = scope.launch { unwrap(e, p) }
            }
            Effect.AnswerApprove -> answerApprove(e)
            Effect.AnswerDeny -> {
                e.answered = true
                e.slot?.answer(ApprovalFfi.Deny)
                e.slot = null
            }
            Effect.CancelPrompt -> e.unwrapJob?.cancel()
            Effect.DiscardCrk -> {
                e.crk?.fill(0)
                e.crk = null
            }
            Effect.CancelServe -> e.serveJob?.cancel()
        }
    }

    /** At scan time, while the app is in the foreground (background starts are refused). */
    private fun startService(e: Entry) {
        supervisor.onPairingStarted(e.id)
        try {
            context.startForegroundService(Intent(context, SessionService::class.java))
        } catch (x: Exception) {
            // E.g. ForegroundServiceStartNotAllowedException: no service, no session.
            post(e, Event.UserCancelled)
        }
    }

    private suspend fun serve(e: Entry) {
        // The vault's previous pairing may still be closing its handle (the
        // container allows one): wait for it.
        closing[e.id]?.join()
        if (e.closed) return
        val handle = try {
            manager.open(e.dir)
        } catch (c: CancellationException) {
            throw c
        } catch (x: Exception) {
            // The vault can't be opened (storage, anti-rollback state).
            post(e, Event.SessionEnded(EndReason.UnlockFailed))
            return
        }
        if (e.closed) {
            handle.close()
            return
        }
        e.handle = handle
        try {
            handle.serveOffer(e.qr, EntryApprover(e), EntryListener(e))
        } catch (c: CancellationException) {
            throw c
        } catch (x: FfiException.MailboxTaken) {
            e.sessionOver.complete(Unit)
            post(e, Event.MailboxTaken)
        } catch (x: FfiException.Timeout) {
            // The relay answered but no browser did in time: an old code.
            e.sessionOver.complete(Unit)
            post(e, Event.OfferExpired)
        } catch (x: Exception) {
            e.sessionOver.complete(Unit)
            post(e, Event.ConnectFailed)
        }
    }

    /**
     * The approval's unlock: a vault whose K_auth needs a re-wrap (screen lock
     * removed, or an unfinished re-wrap) never shows a prompt; otherwise the
     * BiometricPrompt with a CryptoObject on K_auth unwraps the CRK.
     */
    private suspend fun unwrap(e: Entry, presence: UserPresence?) {
        val event = try {
            when {
                presence == null -> Event.UnwrapFailed
                manager.needsRewrap(e.dir) -> Event.UnwrapKeyInvalidated
                else -> {
                    val crk = manager.unwrapCrk(e.dir, presence)
                    e.crk?.fill(0)
                    e.crk = crk
                    Event.CrkUnwrapped
                }
            }
        } catch (c: CancellationException) {
            throw c
        } catch (x: AuthKeyInvalidatedException) {
            Event.UnwrapKeyInvalidated
        } catch (x: PresenceCancelledException) {
            Event.UnwrapCancelled
        } catch (x: Exception) {
            Event.UnwrapFailed
        } finally {
            // The prompt is over: don't keep the screen's activity alive for the session.
            e.presence = null
        }
        post(e, event)
    }

    private fun answerApprove(e: Entry) {
        e.answered = true
        val crk = e.crk
        e.crk = null
        val slot = e.slot
        e.slot = null
        if (crk == null) {
            slot?.answer(ApprovalFfi.Deny)
            return
        }
        if (slot == null) {
            // The prompt is gone (Rust cancelled it); the session's end follows.
            crk.fill(0)
            return
        }
        val answer = ApprovalFfi.Approve(crk)
        e.handed = answer
        slot.answer(answer)
    }

    /** [forced]: Stop, screen off, the service gone: vaults lock at once, without the deny grace. */
    private fun perform(actions: List<SessionSupervisor.Action>, forced: Boolean = false) {
        for (a in actions) {
            when (a) {
                is SessionSupervisor.Action.DenyPending -> entries[a.vaultId]?.let { post(it, Event.Backgrounded) }
                is SessionSupervisor.Action.Lock -> entries.remove(a.vaultId)?.let { close(it, forced) }
                SessionSupervisor.Action.StopService -> service?.shutdown()
            }
        }
        publish()
    }

    /** Locks and releases [e]'s vault: its session ends, the CRK and keys are dropped. */
    private fun close(e: Entry, forced: Boolean) {
        e.closed = true
        e.serveJob?.cancel()
        e.unwrapJob?.cancel()
        e.slot?.answer(ApprovalFfi.Deny)
        e.slot = null
        e.crk?.fill(0)
        e.crk = null
        val h = e.handle
        e.handle = null
        // A pairing that ended with its prompt answered (denied): vault-server
        // tells the browser so itself; locking at once would end the session
        // as "locked" instead. Forced ends (Stop, screen off, a running
        // session) lock at once. The vault was never unlocked in between.
        val graceful = !forced && e.machine.state is State.Ended && e.answered && !e.sessionOver.isCompleted
        val job = scope.launch {
            withContext(NonCancellable) {
                if (graceful) withTimeoutOrNull(ANSWER_GRACE_MS) { e.sessionOver.await() }
                if (h != null) {
                    try {
                        h.lock()
                    } finally {
                        h.close()
                    }
                }
            }
            e.handed?.wipe()
            e.handed = null
            // Usually already reported by the listener; ignored once ended.
            post(e, Event.SessionEnded(EndReason.Locked))
        }
        closing[e.id] = job
        job.invokeOnCompletion { scope.launch { if (closing[e.id] === job) closing.remove(e.id) } }
    }

    private fun publish() {
        runningState.value = entries.values.mapNotNull { e ->
            (e.machine.state as? State.Running)?.let { RunningSession(e.id, e.identity, it.deviceName, e.since) }
        }
        service?.refresh()
    }

    /** vault-ffi's approver for one pairing: the approval screen, then the CRK unwrap. */
    private inner class EntryApprover(private val e: Entry) : ApproverCallback {
        override suspend fun approve(request: ApprovalRequestFfi): ApprovalFfi {
            // The UniFFI call's own job: it completes after the answer was copied to Rust.
            val call = currentCoroutineContext()[Job]
            val slot = ApprovalSlot()
            val answer = try {
                withContext(Dispatchers.Main) { offer(slot, request) }
                slot.await()
            } catch (t: Throwable) {
                // Cancelled by Rust (timeout, session over) or failed: a denial.
                slot.abandon()
                throw t
            }
            if (answer is ApprovalFfi.Approve) call?.invokeOnCompletion { answer.crk.fill(0) }
            return answer
        }

        private fun offer(slot: ApprovalSlot, request: ApprovalRequestFfi) {
            if (e.closed) {
                slot.answer(ApprovalFfi.Deny)
                return
            }
            e.slot?.answer(ApprovalFfi.Deny)
            e.slot = slot
            post(e, Event.ApprovalRequested(request.toCore()))
        }
    }

    /** vault-ffi's session events (Rust threads) onto the main thread. */
    private inner class EntryListener(private val e: Entry) : SessionListener {
        override fun onState(sessionId: ULong, state: SessionStateFfi) {
            val event = when (state) {
                is SessionStateFfi.Active -> Event.SessionActive(sessionId.toLong())
                is SessionStateFfi.Ended -> {
                    e.sessionOver.complete(Unit)
                    Event.SessionEnded(state.reason.toCore())
                }
            }
            scope.launch { post(e, event) }
        }
    }
}
