// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import dev.vault.android.pairing.PairingView
import dev.vault.core.ApprovalRequest
import dev.vault.core.Offer
import dev.vault.core.PairingMachine.State
import dev.vault.core.PairingOutcome
import dev.vault.core.Untrusted

/**
 * A pairing from the scan to the session: waiting for the browser, the
 * approval (app id, origin, relay host, device and code; a mailbox-taken
 * warning blocks approval), the unlock prompt, the running session and how it
 * ended. Everything but the code comes from the browser or the QR: shown as
 * plain text, never trusted.
 */
@Composable
fun PairingScreen(
    view: PairingView,
    onApprove: () -> Unit,
    onDeny: () -> Unit,
    onStop: (vaultId: String) -> Unit,
    onCreate: (Offer) -> Unit,
    onRewrap: (vaultId: String) -> Unit,
    onDone: () -> Unit,
) {
    val s = view.state
    val pending = s is State.Connecting || s is State.AwaitingApproval || s is State.Unlocking
    // Back denies (or cancels) a pending pairing; a running session keeps running.
    BackHandler(enabled = pending) { onDeny() }
    BackHandler(enabled = !pending) { onDone() }
    when (s) {
        State.Idle -> VaultScaffold("Pairing", onBack = onDone) { Working("Starting…") }
        is State.Connecting -> VaultScaffold("Pairing", onBack = null) {
            Working("Waiting for the browser…")
            Identity(s.offer.appId, s.offer.origin)
            Field("Relay", s.offer.relayHost)
            OutlinedButton(onClick = onDeny, modifier = Modifier.fillMaxWidth()) { Text("Cancel") }
        }
        is State.AwaitingApproval -> Approval(s.request, s.canApprove, onApprove, onDeny)
        is State.Unlocking -> VaultScaffold("Approve browser", onBack = null) {
            Working("Confirm with your fingerprint or screen lock…")
            OutlinedButton(onClick = onDeny, modifier = Modifier.fillMaxWidth()) { Text("Deny") }
        }
        is State.Approving -> VaultScaffold("Approve browser", onBack = null) { Working("Unlocking the vault…") }
        is State.Running -> VaultScaffold("Session running", onBack = onDone) {
            Identity(s.vault.appId, s.vault.origin)
            Field("Browser", s.deviceName)
            Text(
                "The vault stays unlocked while the browser uses it. Stop locks it.",
                style = MaterialTheme.typography.bodyMedium,
            )
            Button(onClick = { view.vaultId?.let(onStop) }, modifier = Modifier.fillMaxWidth()) { Text("Stop") }
            OutlinedButton(onClick = onDone, modifier = Modifier.fillMaxWidth()) { Text("Back to the vaults") }
        }
        is State.Ended -> VaultScaffold("Pairing", onBack = onDone) {
            Ended(s.outcome, view.vaultId, onCreate, onRewrap, onDone)
        }
    }
}

/** App id and origin, escaped: an unmatched offer's come straight from the QR. */
@Composable
private fun Identity(appId: String, origin: String) {
    Text(Untrusted.display(origin), style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold)
    Field("App", appId)
}

/** One labelled value, escaped ([Untrusted.display]): every value here is browser- or QR-supplied. */
@Composable
private fun Field(label: String, value: String) {
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium)
        Text(if (value.isEmpty()) "—" else Untrusted.display(value), style = MaterialTheme.typography.bodyLarge)
    }
}

@Composable
private fun Approval(request: ApprovalRequest, canApprove: Boolean, onApprove: () -> Unit, onDeny: () -> Unit) {
    VaultScaffold("Approve browser?", onBack = null) {
        if (request.mailboxTaken) {
            Card(
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.errorContainer,
                    contentColor = MaterialTheme.colorScheme.onErrorContainer,
                ),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("Do not approve", style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.Bold)
                    Text(PairingOutcome.MailboxTaken.message, style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
        Identity(request.appId, request.origin)
        Field("Relay", request.relayHost)
        if (!request.mailboxTaken) {
            Field("Browser", request.deviceName)
            Text("Code", style = MaterialTheme.typography.labelMedium)
            Text(
                request.sas.chunked(3).joinToString(" "),
                style = MaterialTheme.typography.displayMedium,
                fontFamily = FontFamily.Monospace,
            )
            Text(
                "Approve only if the browser shows the same code.",
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = FontWeight.Bold,
            )
        }
        if (canApprove) {
            Button(onClick = onApprove, modifier = Modifier.fillMaxWidth()) { Text("Codes match — approve") }
        }
        OutlinedButton(onClick = onDeny, modifier = Modifier.fillMaxWidth()) { Text("Deny") }
    }
}

@Composable
private fun Ended(
    outcome: PairingOutcome,
    vaultId: String?,
    onCreate: (Offer) -> Unit,
    onRewrap: (String) -> Unit,
    onDone: () -> Unit,
) {
    val offer = when (outcome) {
        is PairingOutcome.NoVaultForApp -> outcome.offer
        is PairingOutcome.WrongApp -> outcome.offer
        else -> null
    }
    if (offer != null) {
        Text("No vault for this app — create one?", style = MaterialTheme.typography.titleLarge)
        Identity(offer.appId, offer.origin)
        Text(
            "Pairing never creates a vault by itself. Create one for this app and origin, then scan the code again.",
            style = MaterialTheme.typography.bodyMedium,
        )
        Button(onClick = { onCreate(offer) }, modifier = Modifier.fillMaxWidth()) { Text("Create a vault for this app") }
    } else {
        Text(outcome.message, style = MaterialTheme.typography.titleMedium)
        if (outcome == PairingOutcome.KeyInvalidated && vaultId != null) {
            Button(onClick = { onRewrap(vaultId) }, modifier = Modifier.fillMaxWidth()) { Text("Re-wrap with the recovery key") }
        }
    }
    OutlinedButton(onClick = onDone, modifier = Modifier.fillMaxWidth()) { Text("Done") }
}

/** "since 14:05" for a running session. */
fun sinceText(millis: Long): String =
    "since " + java.text.DateFormat.getTimeInstance(java.text.DateFormat.SHORT).format(java.util.Date(millis))
