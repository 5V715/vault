// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.Checked
import dev.vault.core.RecoveryConfirmation
import dev.vault.core.VaultCatalog
import java.security.SecureRandom
import kotlin.random.asKotlinRandom
import kotlinx.coroutines.launch

private sealed interface CreateStep {
    data object Form : CreateStep

    data object Working : CreateStep

    /** The recovery key, shown once. */
    class ShowKey(val created: VaultManager.Created) : CreateStep

    class Confirm(val created: VaultManager.Created, val check: RecoveryConfirmation) : CreateStep

    data object Done : CreateStep
}

/**
 * Create: app id + origin (pre-filled after a "No vault for this app" scan) →
 * screen-lock prompt that wraps the CRK → recovery key display → re-enter 4
 * random groups → done. The whole app window is FLAG_SECURE.
 */
@Composable
fun CreateScreen(
    manager: VaultManager,
    catalog: VaultCatalog,
    presence: UserPresence,
    prefillAppId: String,
    prefillOrigin: String,
    onDone: () -> Unit,
    onBack: () -> Unit,
) {
    var step by remember { mutableStateOf<CreateStep>(CreateStep.Form) }
    var appId by remember { mutableStateOf(prefillAppId) }
    var origin by remember { mutableStateOf(prefillOrigin) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    // Leaving before the key is confirmed is allowed only on the form: the vault
    // exists once the key is shown, so Back is hidden from then on.
    val back = if (step == CreateStep.Form) onBack else null
    VaultScaffold("Create a vault", back) {
        when (val s = step) {
            CreateStep.Form -> {
                Text("The app and the web address it runs at. Only this exact pair can use the vault.")
                PlainField("App id", appId, { appId = it }, KeyboardType.Ascii)
                PlainField("Origin (e.g. https://todo.example)", origin, { origin = it })
                error?.let { ErrorText(it) }
                Button(
                    onClick = {
                        when (val id = catalog.checkNew(appId, origin)) {
                            is Checked.Invalid -> error = id.reason
                            is Checked.Valid -> {
                                error = null
                                step = CreateStep.Working
                                scope.launch {
                                    step = try {
                                        CreateStep.ShowKey(manager.create(id.value, presence))
                                    } catch (e: Exception) {
                                        error = e.userMessage()
                                        CreateStep.Form
                                    }
                                }
                            }
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Create") }
            }
            CreateStep.Working -> Working("Creating the vault…")
            is CreateStep.ShowKey -> {
                Text("Recovery key", style = MaterialTheme.typography.titleLarge)
                Text(
                    "Write it down on paper and keep it safe. It is the only way to restore this vault on " +
                        "another phone, or after a new fingerprint or screen lock. It is shown only now.",
                )
                Text(
                    s.created.recoveryKey.split('-').chunked(4).joinToString("\n") { it.joinToString("  ") },
                    style = MaterialTheme.typography.titleMedium.copy(fontFamily = FontFamily.Monospace),
                )
                Button(
                    onClick = {
                        val check = RecoveryConfirmation.start(s.created.recoveryKey, SecureRandom().asKotlinRandom())
                        step = if (check != null) CreateStep.Confirm(s.created, check) else CreateStep.Done
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("I wrote it down") }
            }
            is CreateStep.Confirm -> ConfirmGroups(
                check = s.check,
                onBackToKey = { step = CreateStep.ShowKey(s.created) },
                onConfirmed = { step = CreateStep.Done },
            )
            CreateStep.Done -> {
                Text("The vault is ready.", style = MaterialTheme.typography.titleLarge)
                Text("Scan a pairing code in the browser app to use it.")
                Button(onClick = onDone, modifier = Modifier.fillMaxWidth()) { Text("Done") }
            }
        }
    }
}

@Composable
private fun ConfirmGroups(check: RecoveryConfirmation, onBackToKey: (() -> Unit)?, onConfirmed: () -> Unit) {
    val answers = remember(check) { mutableStateListOf(*Array(check.indices.size) { "" }) }
    var wrong by remember(check) { mutableStateOf(false) }
    Text("Check your copy", style = MaterialTheme.typography.titleLarge)
    Text("Enter these groups from the recovery key you wrote down.")
    check.indices.forEachIndexed { i, idx ->
        RecoveryKeyField("Group ${idx + 1}", answers[i]) { answers[i] = it.take(8) }
    }
    if (wrong) ErrorText("That does not match. Check your copy of the recovery key.")
    Button(
        onClick = { if (check.check(answers.toList())) onConfirmed() else wrong = true },
        modifier = Modifier.fillMaxWidth(),
    ) { Text("Check") }
    onBackToKey?.let { Button(onClick = it) { Text("Show the key again") } }
}
