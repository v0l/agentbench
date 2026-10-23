package io.v0l.agentbench

import android.app.NativeActivity
import android.content.Intent

/** The app's window. It keeps the newest intent, so tapping a notification while the app is open
 *  tells the Rust side which chat to show. Plain NativeActivity ignores new intents. */
class MainActivity : NativeActivity() {
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
    }
}
