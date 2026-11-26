// SPDX-License-Identifier: MIT
package dev.vault.core

/** What started a replication run. */
enum class Trigger { Periodic, AfterSession, Retry, Manual }

/** The background jobs a vault has (a manual run is not a job: it runs in the app). */
enum class JobKind(val trigger: Trigger) {
    Periodic(Trigger.Periodic),
    AfterSession(Trigger.AfterSession),
    Retry(Trigger.Retry),
}

/**
 * The platform's job scheduler, as replication needs it (Android:
 * JobScheduler; later iOS: BGTaskScheduler). Decisions come from
 * [ReplicationPolicy]; an implementation only maps them onto the platform. A
 * job carries nothing but the vault's random id (and its kind).
 */
interface ReplicationJobs {
    /** The persisted run every [ReplicationPolicy.PERIOD_SECS], on any network or unmetered only. */
    fun schedulePeriodic(vaultId: String, wifiOnly: Boolean)

    /** One run of [kind] after [delaySecs] once a network is there; replaces a pending one of that kind. */
    fun scheduleOnce(vaultId: String, kind: JobKind, wifiOnly: Boolean, delaySecs: Long)

    fun cancelAll(vaultId: String)
}

/**
 * Platform job ids from the vault id and the kind: 28 bits of the (random)
 * vault id, times the kinds. Two vaults sharing those 28 bits (odds 2^-28 per
 * pair) would share jobs; the run itself carries the full id.
 */
object ReplicationJobIds {
    private const val KINDS = 4

    fun of(vaultId: String, kind: JobKind): Int = vaultId.take(7).toInt(16) * KINDS + kind.ordinal + 1

    fun kindOf(jobId: Int): JobKind? = if (jobId <= 0) null else JobKind.entries.getOrNull(jobId % KINDS - 1)
}

/**
 * Replica URLs, checked when saved with the rules vault-ffi applies when it
 * replicates: https (http only to this device), a host, no user name or
 * password, no query or fragment.
 */
object ReplicaAddress {
    private val LOOPBACK = setOf("localhost", "127.0.0.1", "[::1]", "::1")

    fun check(url: String): Checked<String> {
        fun bad(why: String) = Checked.Invalid("Replica address: $why")
        val u = try {
            java.net.URI(url.trim())
        } catch (e: java.net.URISyntaxException) {
            return bad("not a URL")
        }
        val host = u.host
        if (host.isNullOrEmpty()) return bad("no host")
        when (u.scheme?.lowercase()) {
            "https" -> Unit
            "http" -> if (host.lowercase() !in LOOPBACK) return bad("https only (http only to this device)")
            else -> return bad("https only")
        }
        if (u.rawUserInfo != null) return bad("no user name or password in the URL (use the fields)")
        if (u.rawQuery != null || u.rawFragment != null) return bad("no ? or # part")
        return Checked.Valid(url.trim())
    }

    /** [url] without any user name or password, for display. */
    fun redacted(url: String): String = try {
        val u = java.net.URI(url)
        if (u.rawUserInfo == null) {
            url
        } else {
            java.net.URI(u.scheme, null, u.host, u.port, u.rawPath, u.rawQuery, u.rawFragment).toString()
        }
    } catch (e: Exception) {
        url.substringAfter('@')
    }
}
