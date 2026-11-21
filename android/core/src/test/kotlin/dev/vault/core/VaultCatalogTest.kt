// SPDX-License-Identifier: MIT
package dev.vault.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class VaultCatalogTest {
    private class FakeRepository : VaultRepository {
        val dirs = linkedMapOf<String, VaultIdentity>()

        override fun list(): List<VaultIdentity> = dirs.values.toList()

        override fun exists(identity: VaultIdentity): Boolean = identity.directoryName in dirs
    }

    private fun id(app: String, origin: String) = (VaultIdentity.of(app, origin) as Checked.Valid).value

    @Test
    fun an_offer_finds_its_vault_by_exact_identity() {
        val repo = FakeRepository()
        val todo = id("todo", "https://todo.example")
        repo.dirs[todo.directoryName] = todo
        val catalog = VaultCatalog(repo)

        assertEquals(todo, catalog.vaultFor(Offer("todo", "https://todo.example", "relay.example")))
        // The LAN showcase is a different vault.
        assertNull(catalog.vaultFor(Offer("todo", "https://192.168.1.20:8443", "relay.example")))
        assertNull(catalog.vaultFor(Offer("notes", "https://todo.example", "relay.example")))
        // Offers carry canonical origins already; anything else is not our vault.
        assertNull(catalog.vaultFor(Offer("todo", "HTTPS://todo.example", "relay.example")))
        assertNull(catalog.vaultFor(Offer("Todo!", "https://todo.example", "relay.example")))
    }

    @Test
    fun the_list_is_sorted_and_create_refuses_duplicates() {
        val repo = FakeRepository()
        val b = id("todo", "https://todo.example")
        val a = id("signal-web", "https://signal.example")
        repo.dirs[b.directoryName] = b
        repo.dirs[a.directoryName] = a
        val catalog = VaultCatalog(repo)
        assertEquals(listOf(a, b), catalog.list())

        // Pre-filled from a WrongApp scan, typed in any case: same vault.
        val dup = catalog.checkNew("todo", "HTTPS://TODO.example:443")
        assertTrue(dup is Checked.Invalid && dup.reason.contains("already"))
        assertEquals(Checked.Valid(id("todo", "https://192.168.1.20:8443")), catalog.checkNew("todo", "https://192.168.1.20:8443"))
        assertTrue(catalog.checkNew("todo", "http://todo.example") is Checked.Invalid)
    }
}
