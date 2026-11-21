// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import dev.vault.android.keys.AuthKeyInvalidatedException
import dev.vault.android.keys.BackgroundKeyMissingException
import dev.vault.android.vault.DuplicateVaultException
import dev.vault.android.vault.PresenceCancelledException
import dev.vault.android.vault.PresenceUnavailableException
import uniffi.vault_ffi.FfiException

/** A screen with a title, a Back action and scrollable content. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun VaultScaffold(title: String, onBack: (() -> Unit)?, content: @Composable ColumnScope.() -> Unit) {
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(title) },
                navigationIcon = { if (onBack != null) TextButton(onClick = onBack) { Text("Back") } },
            )
        },
    ) { padding ->
        Column(
            modifier = Modifier.fillMaxSize().padding(padding).padding(16.dp).verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp),
            content = content,
        )
    }
}

/** Plain text input (app id, origin, URLs): no autocorrect. */
@Composable
fun PlainField(label: String, value: String, onChange: (String) -> Unit, keyboard: KeyboardType = KeyboardType.Uri) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false, keyboardType = keyboard),
        modifier = Modifier.fillMaxWidth(),
    )
}

/** A password or secret key: masked, and the keyboard must not learn it. */
@Composable
fun SecretField(label: String, value: String, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        singleLine = true,
        visualTransformation = PasswordVisualTransformation(),
        keyboardOptions = KeyboardOptions(autoCorrectEnabled = false, keyboardType = KeyboardType.Password),
        modifier = Modifier.fillMaxWidth(),
    )
}

/**
 * Recovery-key input: visible (it is long and checked group by group) but with
 * a password keyboard, so the keyboard neither suggests nor learns it.
 */
@Composable
fun RecoveryKeyField(label: String, value: String, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = value,
        onValueChange = onChange,
        label = { Text(label) },
        textStyle = MaterialTheme.typography.bodyLarge.copy(fontFamily = FontFamily.Monospace),
        keyboardOptions = KeyboardOptions(
            autoCorrectEnabled = false,
            capitalization = KeyboardCapitalization.Characters,
            keyboardType = KeyboardType.Password,
        ),
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
fun Working(text: String) {
    Text(text, style = MaterialTheme.typography.bodyLarge)
    LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
}

@Composable
fun ErrorText(text: String) {
    Text(text, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodyMedium)
}

/** What a failed operation tells the user. Never includes secrets or raw details. */
fun Throwable.userMessage(): String = when (this) {
    is PresenceCancelledException -> "Cancelled at the unlock prompt. Nothing was kept."
    is PresenceUnavailableException ->
        if (locked) {
            "Too many attempts. Unlock the phone with its PIN, then try again."
        } else {
            "Set a screen lock (PIN, pattern or password) first. The vault needs it."
        }
    is BackgroundKeyMissingException -> "This vault's storage key is missing; restore it from a replica."
    is javax.crypto.AEADBadTagException -> "A vault file is damaged or was altered. Restore the vault from a replica."
    is DuplicateVaultException -> message ?: "This vault already exists."
    is AuthKeyInvalidatedException -> "The unlock key was invalidated because the screen lock was removed. Re-wrap with the recovery key."
    is FfiException.RecoveryKeyTypo -> "The recovery key has a typo (checksum mismatch)."
    is FfiException.WrongKey -> "This recovery key does not open this vault, or the data is damaged."
    is FfiException.NotFound -> "No vault was found at this location."
    is FfiException.Rollback, is FfiException.RollbackDetected ->
        "The stored vault is older than its last known state (rollback). Nothing was changed."
    is FfiException.HighWaterMissing -> "The vault's anti-rollback record is missing. Restore it from a replica."
    is FfiException.Diverged -> "The replicas hold different versions of this vault."
    is FfiException.Tls -> "Secure connection failed (certificate)."
    is FfiException.Timeout -> "The connection timed out."
    is FfiException.Storage -> "Storage error. Check the address and credentials."
    is FfiException.Unsupported -> "This vault's format is not supported by this app version."
    is FfiException.InvalidInput -> "Some input is not valid. Check the address and fields."
    is FfiException -> "The vault operation failed (${javaClass.simpleName})."
    else -> "Something went wrong (${javaClass.simpleName})."
}
