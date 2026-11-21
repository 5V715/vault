// SPDX-License-Identifier: MIT
package dev.vault.android

import android.content.Context
import android.os.Build
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.view.WindowManager
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import dev.vault.android.keys.AuthKeyInvalidatedException
import dev.vault.android.keys.VaultKeys
import dev.vault.android.storage.ReplicaConfig
import dev.vault.android.storage.VaultDir
import dev.vault.android.storage.VaultStorage
import dev.vault.android.vault.PresenceCancelledException
import dev.vault.android.vault.UserPresence
import dev.vault.android.vault.VaultManager
import dev.vault.core.Checked
import dev.vault.core.VaultIdentity
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.AfterClass
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.BeforeClass
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.vault_ffi.NamedReplica
import uniffi.vault_ffi.ReplicaOutcomeFfi
import uniffi.vault_ffi.ReplicaTarget

/** Passes every prompt without a user: test keys need no authentication. */
private class AutoPresence(var pass: Boolean = true) : UserPresence {
    var prompts = 0

    override suspend fun authenticate(title: String, cipher: Cipher): Cipher? {
        prompts++
        return cipher.takeIf { pass }
    }

    override suspend fun confirm(title: String): Boolean = pass
}

/**
 * Keystore keys, sealed files and the create / restore / re-wrap flows on a
 * device. Emulators can't pass a BiometricPrompt, so the flows use
 * `VaultKeys(requireAuth = false)`: K_auth's authentication requirement is
 * checked only by its parameters (k_auth_params_correct) and on a real phone
 * (the owner's manual checklist). That gap is deliberate and documented.
 */
@RunWith(AndroidJUnit4::class)
class VaultStorageTest {
    private val context: Context = InstrumentationRegistry.getInstrumentation().targetContext
    private lateinit var keys: VaultKeys
    private lateinit var storage: VaultStorage
    private lateinit var manager: VaultManager
    private val identity = (VaultIdentity.of("todo", "https://todo.example") as Checked.Valid).value

    @Before
    fun setUp() {
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        keys = VaultKeys(context, requireAuth = false)
        storage = VaultStorage(context, keys)
        manager = VaultManager(storage, keys)
    }

    @After
    fun tearDown() {
        File(context.noBackupFilesDir, "vaults").deleteRecursively()
        // Also the keys of vaults a failing test left incomplete.
        keys.vaultIdsWithKeys().forEach(keys::deleteKeys)
        manager.close()
    }

    private fun id(app: String, origin: String) = (VaultIdentity.of(app, origin) as Checked.Valid).value

    private fun keyInfo(alias: String): KeyInfo {
        val ks = KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }
        val key = ks.getKey(alias, null) as SecretKey
        return SecretKeyFactory.getInstance(key.algorithm, VaultKeys.ANDROID_KEYSTORE)
            .getKeySpec(key, KeyInfo::class.java) as KeyInfo
    }

    /** Create and confirm (complete) a vault. */
    private suspend fun createComplete(id: VaultIdentity = identity, presence: UserPresence = AutoPresence()): VaultManager.Created =
        manager.create(id, presence).also { manager.confirmCreated(it) }

    private fun aliases(): List<String> =
        KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.aliases().toList()

    @Test
    fun k_auth_params_correct() {
        // The real (authenticated) key class, as the app creates it.
        val real = VaultKeys(context)
        val id = "0123456789abcdef0123456789abcdef"
        try {
            real.createAuthKey(id)
            val info = keyInfo(real.authAlias(id))
            assertEquals(256, info.keySize)
            assertEquals(KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT, info.purposes)
            assertArrayEquals(arrayOf(KeyProperties.BLOCK_MODE_GCM), info.blockModes)
            assertTrue(info.isUserAuthenticationRequired)
            if (Build.VERSION.SDK_INT >= 31) assertTrue(info.isUnlockedDeviceRequired)
            // Timeout 0: every use needs its own prompt with a CryptoObject.
            assertEquals(0, info.userAuthenticationValidityDurationSeconds)
            assertEquals(
                KeyProperties.AUTH_BIOMETRIC_STRONG or KeyProperties.AUTH_DEVICE_CREDENTIAL,
                info.userAuthenticationType,
            )
            // Not asserted: isInvalidatedByBiometricEnrollment. A key that the
            // device credential also unlocks is not invalidated by a new
            // biometric (owner decision 2026-11-21: fingerprint or PIN).
            // Usable means "not invalidated": the key still needs a prompt.
            assertTrue(real.isAuthKeyUsable(id))
            // Without a prompt the key does not encrypt.
            val c = real.authCipherForWrap(id)
            try {
                c.doFinal(ByteArray(32))
                fail("K_auth encrypted without user authentication")
            } catch (e: Exception) {
                // expected: IllegalBlockSizeException wrapping UserNotAuthenticatedException
            }
        } finally {
            real.deleteKeys(id)
        }
    }

    @Test
    fun k_bg_params_correct() {
        val id = "fedcba9876543210fedcba9876543210"
        try {
            keys.createBackgroundKey(id)
            val info = keyInfo(keys.bgAlias(id))
            assertEquals(256, info.keySize)
            assertArrayEquals(arrayOf(KeyProperties.BLOCK_MODE_GCM), info.blockModes)
            // No user authentication: replication and the high-water mark need it
            // while the vault is locked; only an unlocked device (checked on API 31+).
            assertFalse(info.isUserAuthenticationRequired)
            if (Build.VERSION.SDK_INT >= 31) assertTrue(info.isUnlockedDeviceRequired)

            val secret = ByteArray(32) { it.toByte() }
            val sealed = keys.sealBackground(id, VaultDir.KIND_MANIFEST_KEY, secret.copyOf())
            assertArrayEquals(secret, keys.openBackground(id, VaultDir.KIND_MANIFEST_KEY, sealed))
            // Bound to its kind and vault, and tamper-evident.
            assertThrows { keys.openBackground(id, VaultDir.KIND_HIGH_WATER, sealed) }
            keys.createBackgroundKey("00000000000000000000000000000000")
            assertThrows { keys.openBackground("00000000000000000000000000000000", VaultDir.KIND_MANIFEST_KEY, sealed) }
            val flipped = sealed.copyOf().also { it[it.size - 1] = (it[it.size - 1].toInt() xor 1).toByte() }
            assertThrows { keys.openBackground(id, VaultDir.KIND_MANIFEST_KEY, flipped) }
            // Two seals of the same plaintext differ (fresh IV).
            assertFalse(sealed.contentEquals(keys.sealBackground(id, VaultDir.KIND_MANIFEST_KEY, secret.copyOf())))
        } finally {
            keys.deleteKeys(id)
            keys.deleteKeys("00000000000000000000000000000000")
        }
    }

    @Test
    fun wrap_unwrap_crk_with_device_credential_test_key() = runBlocking<Unit> {
        val presence = AutoPresence()
        val created = createComplete(presence = presence)
        val crk = manager.unwrapCrk(created.dir, presence)
        try {
            assertEquals(32, crk.size)
            // The recovery key re-derives the same CRK the wrapped file holds.
            val again = manager.factory.rewrapWithRecoveryKey(created.dir.container.path, created.recoveryKey, storage.highWaterStore(created.dir))
            assertArrayEquals(crk, again)
            again.fill(0)
            // The wrapped file is not the CRK and is bound to this vault.
            val wrapped = created.dir.wrappedCrk.readBytes()
            assertFalse(wrapped.indexOfSlice(crk) >= 0)
            assertThrows { keys.unwrapCrk("ffffffffffffffffffffffffffffffff", keys.authCipherForUnwrap(created.dir.id, wrapped), wrapped) }
        } finally {
            crk.fill(0)
        }
        // A cancelled prompt yields no CRK.
        try {
            manager.unwrapCrk(created.dir, AutoPresence(pass = false))
            fail("unwrapped without passing the prompt")
        } catch (e: PresenceCancelledException) {
            // expected
        }
    }

    @Test
    fun rewrap_after_key_invalidation() = runBlocking<Unit> {
        val presence = AutoPresence()
        val created = createComplete(presence = presence)
        val before = manager.unwrapCrk(created.dir, presence)
        assertFalse(manager.needsRewrap(created.dir))

        // Removing the screen lock invalidates K_auth; on an
        // emulator the closest stand-in is the key disappearing.
        KeyStore.getInstance(VaultKeys.ANDROID_KEYSTORE).apply { load(null) }.deleteEntry(keys.authAlias(created.dir.id))
        assertTrue(manager.needsRewrap(created.dir))
        try {
            manager.unwrapCrk(created.dir, presence)
            fail("unwrapped with an invalidated key")
        } catch (e: AuthKeyInvalidatedException) {
            // expected
        }
        // A wrong recovery key (a valid one, of another vault) changes nothing.
        val other = createComplete(id("notes", "https://notes.example"), presence)
        try {
            manager.rewrap(created.dir, other.recoveryKey, presence)
            fail("re-wrapped with another vault's recovery key")
        } catch (e: uniffi.vault_ffi.FfiException.WrongKey) {
            assertTrue(manager.needsRewrap(created.dir))
        }
        // Cancelling the prompt keeps the vault in "needs re-wrap" (and keeps the
        // re-wrap entry visible) instead of leaving a fresh, useless key.
        try {
            manager.rewrap(created.dir, created.recoveryKey, AutoPresence(pass = false))
            fail("re-wrapped without passing the prompt")
        } catch (e: PresenceCancelledException) {
            assertTrue(manager.needsRewrap(created.dir))
        }

        manager.rewrap(created.dir, created.recoveryKey, presence)
        assertFalse(manager.needsRewrap(created.dir))
        val after = manager.unwrapCrk(created.dir, presence)
        assertArrayEquals(before, after)
        before.fill(0)
        after.fill(0)
    }

    @Test
    fun files_in_no_backup_dir() = runBlocking<Unit> {
        val outside = snapshot(context.dataDir)
        val created = createComplete()

        // Everything the vault wrote is under noBackupFilesDir/vaults/<id>/.
        val root = created.dir.root.canonicalFile
        assertEquals(File(context.noBackupFilesDir, "vaults").canonicalFile, root.parentFile)
        assertTrue(Regex("^[0-9a-f]{32}$").matches(root.name))
        val added = snapshot(context.dataDir) - outside
        assertTrue(added.toString(), added.all { it.startsWith(root.path + File.separator) })
        assertEquals(
            setOf("container", "crk.wrapped", "identity.sealed", "high-water.sealed", "manifest-key.sealed"),
            root.list()!!.toSet(),
        )
        // The identity is sealed, not in the clear.
        assertFalse(String(File(root, "identity.sealed").readBytes(), Charsets.ISO_8859_1).contains("todo.example"))
        assertEquals(listOf(identity), storage.list())
        assertEquals(created.dir.id, storage.dirOf(identity)?.id)
        // A second vault for the same identity is refused before anything is written.
        try {
            manager.create(identity, AutoPresence())
            fail("created a duplicate vault")
        } catch (e: Exception) {
            assertEquals(1, storage.vaultDirs().size)
        }
    }

    @Test
    fun cancelled_create_leaves_nothing() = runBlocking<Unit> {
        val before = aliases().toSet()
        try {
            manager.create(identity, AutoPresence(pass = false))
            fail("created without passing the prompt")
        } catch (e: PresenceCancelledException) {
            // expected
        }
        assertEquals(emptyList<String>(), File(context.noBackupFilesDir, "vaults").list()?.toList() ?: emptyList<String>())
        assertEquals(before, aliases().toSet())
        assertFalse(storage.exists(identity))
    }

    @Test
    fun restore_from_a_replica_and_keep_its_credentials() = runBlocking<Unit> {
        val presence = AutoPresence()
        val created = createComplete(presence = presence)
        val crk = manager.unwrapCrk(created.dir, presence)
        val replicaDir = File(context.noBackupFilesDir, "replica-test").apply { deleteRecursively() }
        try {
            // Replicate with the sealed manifest key only (locked vault).
            val target = ReplicaTarget.Local(replicaDir.path)
            manager.open(created.dir).use { handle ->
                assertTrue(handle.isLocked())
                val mk = storage.readSealed(created.dir, VaultDir.KIND_MANIFEST_KEY)!!
                val results = handle.replicate(mk, listOf(NamedReplica("r1", target)))
                mk.fill(0)
                assertTrue(results.single().outcome is ReplicaOutcomeFfi.Synced)
            }
            // The phone is lost: only the replica and the recovery key remain.
            storage.delete(created.dir)
            assertFalse(storage.exists(identity))

            val restored = manager.restore(target, created.recoveryKey, presence)
            assertEquals(identity, restored)
            val dir = storage.dirOf(identity)!!
            val again = manager.unwrapCrk(dir, presence)
            assertArrayEquals(crk, again)
            again.fill(0)
            // The credentials typed for the restore are kept, sealed, for replication.
            val replicas = ReplicaConfig.decode(storage.readSealed(dir, VaultDir.KIND_REPLICAS)!!)
            assertEquals(ReplicaTarget.Local(replicaDir.path), replicas.single().target)
            assertNotNull(storage.readSealed(dir, VaultDir.KIND_MANIFEST_KEY))
            // Restoring the same vault again is refused, and leaves nothing behind.
            try {
                manager.restore(target, created.recoveryKey, presence)
                fail("restored a duplicate")
            } catch (e: Exception) {
                assertEquals(1, storage.vaultDirs().size)
                assertEquals(1, File(context.noBackupFilesDir, "vaults").list()!!.size)
            }
            // A wrong recovery key (valid, of another vault) fails after the
            // download started, and leaves nothing behind either.
            val otherKey = createComplete(id("notes", "https://notes.example"), presence).recoveryKey
            storage.vaultDirs().forEach(storage::delete)
            val aliasesBefore = aliases().toSet()
            try {
                manager.restore(target, otherKey, presence)
                fail("restored with another vault's recovery key")
            } catch (e: uniffi.vault_ffi.FfiException) {
                assertNull(File(context.noBackupFilesDir, "vaults").list()?.firstOrNull())
                assertEquals(aliasesBefore, aliases().toSet())
            }
        } finally {
            crk.fill(0)
            replicaDir.deleteRecursively()
        }
    }

    @Test
    fun unconfirmed_and_interrupted_vaults_are_swept() = runBlocking<Unit> {
        // Created, recovery key never confirmed (app killed on the key screen).
        val unconfirmed = manager.create(identity, AutoPresence())
        assertFalse(storage.exists(identity))
        assertTrue(unconfirmed.dir.root.exists())
        // A key whose vault directory is gone (cleanup raced a cancelled task).
        keys.createBackgroundKey("abcdefabcdefabcdefabcdefabcdefab")
        val complete = createComplete(id("notes", "https://notes.example"))

        storage.sweepIncomplete()
        assertFalse(unconfirmed.dir.root.exists())
        assertFalse(keys.vaultIdsWithKeys().contains(unconfirmed.dir.id))
        assertFalse(keys.vaultIdsWithKeys().contains("abcdefabcdefabcdefabcdefabcdefab"))
        // Complete vaults are untouched.
        assertTrue(complete.dir.isComplete)
        assertTrue(keys.vaultIdsWithKeys().contains(complete.dir.id))
        // And the identity can be created again.
        createComplete()
        assertEquals(2, storage.list().size)
    }

    @Test
    fun cancelled_coroutine_leaves_nothing() = runBlocking<Unit> {
        val before = aliases().toSet()
        // The prompt never answers; the user leaves the screen.
        val waiting = object : UserPresence {
            override suspend fun authenticate(title: String, cipher: Cipher): Cipher? = kotlinx.coroutines.awaitCancellation()

            override suspend fun confirm(title: String): Boolean = false
        }
        val job = launch(kotlinx.coroutines.Dispatchers.Default) { manager.create(identity, waiting) }
        // Wait until the create reached the prompt (the container exists).
        withTimeout(30_000) {
            while (File(context.noBackupFilesDir, "vaults").listFiles()?.any { File(it, "crk.wrapped").exists() || File(it, "manifest-key.sealed").exists() } != true) delay(20)
        }
        job.cancelAndJoin()
        assertNull(File(context.noBackupFilesDir, "vaults").list()?.firstOrNull())
        assertEquals(before, aliases().toSet())
    }

    @Test
    fun replica_config_round_trip() {
        val replicas = listOf(
            NamedReplica("a1", ReplicaTarget.S3("https://s3.example", "eu-central-1", "b", "p/", "AKID", "s3cr\"et\n", true)),
            NamedReplica("b2", ReplicaTarget.WebDav("https://cloud.example/remote.php/dav/files/u/vault", "u", "pässwörd")),
            NamedReplica("c3", ReplicaTarget.Local("/data/x")),
        )
        assertEquals(replicas, ReplicaConfig.decode(ReplicaConfig.encode(replicas)))
        assertThrows { ReplicaConfig.decode("{\"v\":2,\"replicas\":[]}".toByteArray()) }
    }

    @Test
    fun flag_secure_on_secret_screens() {
        // Every screen of the app is secret-bearing, so the whole window is secure.
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            scenario.onActivity { activity ->
                val flags = activity.window.attributes.flags
                assertTrue(flags and WindowManager.LayoutParams.FLAG_SECURE != 0)
            }
        }
    }

    private fun snapshot(root: File): Set<String> =
        root.walkTopDown()
            .filter { it.isFile }
            .map { it.canonicalPath }
            // Runtime caches the platform and the test harness write.
            // The ART/JIT cache; everything else (cacheDir included) counts.
            .filterNot { "/code_cache/" in it }
            .toSet()

    private fun ByteArray.indexOfSlice(needle: ByteArray): Int =
        (0..size - needle.size).firstOrNull { i -> needle.indices.all { this[i + it] == needle[it] } } ?: -1

    private inline fun assertThrows(block: () -> Unit) {
        try {
            block()
        } catch (e: Exception) {
            return
        }
        fail("expected an exception")
    }

    companion object {
        private const val PIN = "1234"

        private fun shell(cmd: String) {
            val ui = InstrumentationRegistry.getInstrumentation().uiAutomation
            ui.executeShellCommand(cmd).use { pfd ->
                android.os.ParcelFileDescriptor.AutoCloseInputStream(pfd).readBytes()
            }
        }

        /** Keys bound to user authentication need a secure lock screen. */
        @BeforeClass
        @JvmStatic
        fun setPin() = shell("locksettings set-pin $PIN")

        @AfterClass
        @JvmStatic
        fun clearPin() = shell("locksettings clear --old $PIN")
    }
}
