// SPDX-License-Identifier: MIT
package dev.vault.android.storage

import org.json.JSONArray
import org.json.JSONObject
import uniffi.vault_ffi.NamedReplica
import uniffi.vault_ffi.ReplicaTarget

/**
 * A vault's replica targets with their credentials, as JSON. It lives only
 * sealed under K_bg (`replicas.sealed`), never in the clear and never in the
 * container (spec, background replication option B).
 */
object ReplicaConfig {
    private const val VERSION = 1

    fun encode(replicas: List<NamedReplica>): ByteArray {
        val arr = JSONArray()
        for (r in replicas) {
            val o = JSONObject().put("id", r.id)
            when (val t = r.target) {
                is ReplicaTarget.Local -> o.put("type", "local").put("path", t.path)
                is ReplicaTarget.S3 -> o.put("type", "s3")
                    .put("endpoint", t.endpoint)
                    .put("region", t.region)
                    .put("bucket", t.bucket)
                    .put("prefix", t.prefix)
                    .put("accessKeyId", t.accessKeyId)
                    .put("secretAccessKey", t.secretAccessKey)
                    .put("pathStyle", t.pathStyle)
                is ReplicaTarget.WebDav -> o.put("type", "webdav")
                    .put("url", t.url)
                    .put("user", t.user)
                    .put("password", t.password)
            }
            arr.put(o)
        }
        return JSONObject().put("v", VERSION).put("replicas", arr).toString().toByteArray(Charsets.UTF_8)
    }

    /** Parses [encode]'s output; throws on anything else. */
    fun decode(bytes: ByteArray): List<NamedReplica> {
        val root = JSONObject(String(bytes, Charsets.UTF_8))
        require(root.getInt("v") == VERSION) { "unknown replica config version" }
        val arr = root.getJSONArray("replicas")
        return (0 until arr.length()).map { i ->
            val o = arr.getJSONObject(i)
            val target = when (o.getString("type")) {
                "local" -> ReplicaTarget.Local(o.getString("path"))
                "s3" -> ReplicaTarget.S3(
                    o.getString("endpoint"),
                    o.getString("region"),
                    o.getString("bucket"),
                    o.getString("prefix"),
                    o.getString("accessKeyId"),
                    o.getString("secretAccessKey"),
                    o.getBoolean("pathStyle"),
                )
                "webdav" -> ReplicaTarget.WebDav(o.getString("url"), o.getString("user"), o.getString("password"))
                else -> throw IllegalArgumentException("unknown replica type")
            }
            NamedReplica(o.getString("id"), target)
        }
    }
}
