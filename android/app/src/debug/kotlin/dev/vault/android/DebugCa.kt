// SPDX-License-Identifier: MIT
package dev.vault.android

import android.content.Context
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.writeAtomically
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.vault_ffi.FactoryConfig
import uniffi.vault_ffi.FfiException
import uniffi.vault_ffi.VaultFactory

/**
 * Debug builds only: one extra CA certificate (PEM) that Rust trusts for the
 * relay and the replicas, for the LAN showcase's local CA (vault-dev's
 * `--ca-file`). It is sealed under its own background Keystore key in
 * noBackupFilesDir and passed as `FactoryConfig.extraCaPem` when the app
 * starts. Release builds contain none of this (src/release has a stub).
 */
object DebugCa {
    /** The import button's label; check-apks.sh looks for it in the dex files. */
    const val IMPORT_LABEL = "Import a CA certificate (PEM)"

    private const val KEY_ID = "debug-ca"
    private const val KIND = "extra-ca"
    private const val MAX_BYTES = 64 * 1024

    private fun file(context: Context) = File(context.noBackupFilesDir, "debug-extra-ca.sealed")

    /** The imported PEM, or null (none, or it can't be unsealed). */
    fun extraCaPem(context: Context, keys: VaultKeys): String? = try {
        val f = file(context)
        if (f.isFile) keys.openBackground(KEY_ID, KIND, f.readBytes()).toString(Charsets.UTF_8) else null
    } catch (e: Exception) {
        null
    }

    /** Reads, checks and seals the PEM at [uri]; returns the message to show. */
    private fun importPem(context: Context, keys: VaultKeys, uri: Uri): String {
        val bytes = try {
            context.contentResolver.openInputStream(uri)?.use { input ->
                val buf = ByteArray(MAX_BYTES + 1)
                var n = 0
                while (n < buf.size) {
                    val r = input.read(buf, n, buf.size - n)
                    if (r < 0) break
                    n += r
                }
                buf.copyOf(n)
            }
        } catch (e: Exception) {
            null
        } ?: return "The file could not be read."
        if (bytes.size > MAX_BYTES) return "The file is too large for a CA certificate."
        val pem = bytes.toString(Charsets.UTF_8)
        if (!pem.contains("-----BEGIN CERTIFICATE-----")) return "This is not a PEM certificate."
        // vault-ffi refuses a PEM without a usable certificate.
        try {
            VaultFactory(FactoryConfig(extraCaPem = pem, approvalTimeoutSecs = 60uL, idleTimeoutSecs = null)).close()
        } catch (e: FfiException) {
            return "The certificate was not accepted."
        }
        keys.createBackgroundKey(KEY_ID)
        writeAtomically(file(context), keys.sealBackground(KEY_ID, KIND, bytes))
        return "CA imported. Restart the app to use it."
    }

    private fun clear(context: Context, keys: VaultKeys) {
        file(context).delete()
        keys.deleteKeys(KEY_ID)
    }

    /** The setting on the vault list (debug builds only). */
    @Composable
    fun Setting(keys: VaultKeys) {
        val context = LocalContext.current
        val scope = rememberCoroutineScope()
        var installed by remember { mutableStateOf(file(context).isFile) }
        var status by remember { mutableStateOf<String?>(null) }
        val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
            if (uri != null) {
                scope.launch {
                    status = withContext(Dispatchers.IO) { importPem(context, keys, uri) }
                    installed = file(context).isFile
                }
            }
        }
        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text("Debug build: extra CA for the LAN showcase", style = MaterialTheme.typography.titleSmall)
            Text(
                if (installed) "An extra CA is installed (applies from the next app start)." else "Only the bundled roots are trusted.",
                style = MaterialTheme.typography.bodySmall,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = { picker.launch(arrayOf("*/*")) }) { Text(IMPORT_LABEL) }
                if (installed) {
                    TextButton(onClick = {
                        scope.launch {
                            withContext(Dispatchers.IO) { clear(context, keys) }
                            installed = false
                            status = "Removed. Restart the app to apply."
                        }
                    }) { Text("Remove") }
                }
            }
            status?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
    }
}
