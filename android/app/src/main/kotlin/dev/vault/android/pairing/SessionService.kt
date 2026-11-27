// SPDX-License-Identifier: MIT
package dev.vault.android.pairing

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.graphics.drawable.Icon
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock
import dev.vault.android.MainActivity
import dev.vault.android.R
import dev.vault.android.VaultApp
import dev.vault.core.SessionNotice
import java.lang.ref.WeakReference
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * The foreground service (type `specialUse`) that keeps paired browser
 * sessions alive. It starts at scan time, shows the app ids and origins being
 * served with a Stop action that locks everything, and stops when no vault
 * pairs or serves. Its destruction locks every vault ([PairingCoordinator]
 * holds the vault handles only while this service is wanted).
 *
 * Not sticky: after process death nothing is restarted; every vault is
 * locked because nothing about a session is persisted.
 */
class SessionService : Service() {
    private lateinit var coordinator: PairingCoordinator

    /** [shutdown] was asked for (nothing left to hold), as opposed to a system stop. */
    private var stopping = false

    // Notification updates are throttled: Android drops ("sheds") updates from an app
    // that posts more than about 5 per second, and a dropped last update leaves a stale
    // notification (CI run 46031365491, API 36: the Running notification never showed).
    private val handler = Handler(Looper.getMainLooper())
    private var lastPostMs = 0L
    private var refreshQueued = false

    private val screenOff = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action == Intent.ACTION_SCREEN_OFF) coordinator.onScreenOff()
        }
    }

    override fun onCreate() {
        super.onCreate()
        coordinator = (application as VaultApp).pairing
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL, "Vault sessions", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Shown while a browser is paired with a vault"
                setShowBadge(false)
                lockscreenVisibility = Notification.VISIBILITY_PRIVATE
            },
        )
        val filter = IntentFilter(Intent.ACTION_SCREEN_OFF)
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(screenOff, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(screenOff, filter)
        }
        coordinator.attach(this)
        live = WeakReference(this)
        runningState.value = true
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Foreground first, on every start: a service started with
        // startForegroundService must call startForeground, even to stop.
        goForeground()
        if (intent?.action == ACTION_STOP) coordinator.stopAll()
        // Nothing to hold (the pairing already ended, or a Stop): go away.
        if (coordinator.isIdle) shutdown()
        return START_MODE
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        unregisterReceiver(screenOff)
        coordinator.detach(this, requested = stopping)
        if (live?.get() === this) live = null
        runningState.value = false
        super.onDestroy()
    }

    /** Removes the notification and stops (main thread). */
    internal fun shutdown() {
        stopping = true
        stopForeground(Service.STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    /** Shows the current sessions in the notification (main thread). */
    internal fun refresh() {
        // Stopping: re-posting would leave a notification behind.
        if (stopping) return
        // Through startForeground, not NotificationManager.notify: a plain
        // re-post loses FLAG_FOREGROUND_SERVICE on Android 11 (the
        // notification becomes dismissable and detached from the service).
        // At most one update per MIN_UPDATE_MS; a refresh inside that window is
        // posted at its end (trailing), so the latest state always shows.
        val wait = lastPostMs + MIN_UPDATE_MS - SystemClock.uptimeMillis()
        if (wait <= 0) {
            goForeground()
        } else if (!refreshQueued) {
            refreshQueued = true
            handler.postDelayed({
                refreshQueued = false
                if (!stopping) goForeground()
            }, wait)
        }
    }

    private fun goForeground() {
        lastPostMs = SystemClock.uptimeMillis()
        val n = notification()
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIFICATION_ID, n)
        }
    }

    private fun notification(): Notification {
        val notice = coordinator.notice()
        val stop = PendingIntent.getService(
            this,
            0,
            Intent(this, SessionService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val icon = Icon.createWithResource(this, R.drawable.ic_session)
        // The lock screen sees neither the app nor the origin.
        val lockScreen = Notification.Builder(this, CHANNEL)
            .setSmallIcon(icon)
            .setContentTitle(SessionNotice.PUBLIC_TITLE)
            .build()
        val b = Notification.Builder(this, CHANNEL)
            .setSmallIcon(icon)
            .setContentTitle(notice.title)
            .setContentText(notice.lines.firstOrNull().orEmpty())
            .setStyle(Notification.BigTextStyle().bigText(notice.text))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(false)
            .setCategory(Notification.CATEGORY_SERVICE)
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(lockScreen)
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(icon, STOP_LABEL, stop).build())
        if (Build.VERSION.SDK_INT >= 31) b.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
        return b.build()
    }

    companion object {
        const val ACTION_STOP = "dev.vault.android.action.STOP_SESSIONS"
        const val NOTIFICATION_ID = 1

        /** The shortest gap between two notification updates (Android sheds more than ~5/s). */
        const val MIN_UPDATE_MS = 300L
        const val CHANNEL = "sessions"
        const val STOP_LABEL = "Stop"

        /** Never restarted by the system: a restart would hold nothing. */
        const val START_MODE = Service.START_NOT_STICKY

        private val runningState = MutableStateFlow(false)

        @Volatile
        private var live: WeakReference<SessionService>? = null

        /**
         * The running instance's foreground service type as the system holds
         * it (0 when not in the foreground), or null without an instance.
         */
        fun liveForegroundServiceType(): Int? = live?.get()?.foregroundServiceType

        /** Whether the service exists (between onCreate and onDestroy). */
        val running: StateFlow<Boolean> = runningState.asStateFlow()
    }
}
