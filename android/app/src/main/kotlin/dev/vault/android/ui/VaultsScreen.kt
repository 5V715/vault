// SPDX-License-Identifier: MIT
package dev.vault.android.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import dev.vault.android.R
import dev.vault.android.storage.VaultDir
import dev.vault.core.VaultIdentity

/** A vault on this phone: its files and its identity. */
data class VaultEntry(val dir: VaultDir, val identity: VaultIdentity)

/** The start screen: every vault on this phone, by app id and origin. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun VaultsScreen(
    vaults: List<VaultEntry>?,
    onOpen: (VaultEntry) -> Unit,
    onCreate: () -> Unit,
    onRestore: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Scaffold(
        modifier = modifier,
        topBar = { TopAppBar(title = { Text(stringResource(R.string.vaults_title)) }) },
    ) { padding ->
        Column(modifier = Modifier.fillMaxSize().padding(padding)) {
            when {
                vaults == null -> Column(Modifier.padding(16.dp)) { Working("Loading…") }
                vaults.isEmpty() -> Text(
                    text = stringResource(R.string.vaults_empty),
                    style = MaterialTheme.typography.bodyLarge,
                    modifier = Modifier.padding(24.dp),
                )
                else -> LazyColumn(modifier = Modifier.weight(1f)) {
                    items(vaults, key = { it.dir.id }) { vault ->
                        ListItem(
                            headlineContent = { Text(vault.identity.appId) },
                            supportingContent = { Text(vault.identity.origin) },
                            modifier = Modifier.clickable { onOpen(vault) },
                        )
                        HorizontalDivider()
                    }
                }
            }
            Column(
                modifier = Modifier.fillMaxWidth().padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Button(onClick = onCreate, modifier = Modifier.fillMaxWidth()) { Text("Create a vault") }
                OutlinedButton(onClick = onRestore, modifier = Modifier.fillMaxWidth()) { Text("Restore a vault") }
            }
        }
    }
}
