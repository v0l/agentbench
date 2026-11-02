package io.v0l.agentbench

import android.app.NativeActivity
import android.content.Intent
import android.content.ContentValues
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import java.io.ByteArrayOutputStream
import java.util.concurrent.ConcurrentLinkedQueue

/** One file the user picked. `data` is null when it could not be read or is too big. */
class Picked(val name: String, val data: ByteArray?)

/** The app's window. It keeps the newest intent, so tapping a notification while the app is open
 *  tells the Rust side which chat to show. Plain NativeActivity ignores new intents. It also
 *  opens the system file picker and hands the chosen files to the Rust side to take. */
class MainActivity : NativeActivity() {
    private val picked = ConcurrentLinkedQueue<Picked>()
    @Volatile private var reading = 0

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
    }

    fun pickFiles() {
        runOnUiThread {
            val open = Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
                addCategory(Intent.CATEGORY_OPENABLE)
                type = "*/*"
                putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
            }
            reading += 1
            try {
                startActivityForResult(open, PICK)
            } catch (e: Exception) {
                reading -= 1
                picked.add(Picked("file picker", null))
            }
        }
    }

    /** Keep a file in the Downloads folder, and open it in another app when `view`. Returns an
     *  empty string on success, otherwise what went wrong. */
    fun saveDownload(name: String, mime: String, data: ByteArray, view: Boolean): String {
        if (Build.VERSION.SDK_INT < 29) return "saving needs Android 10 or newer"
        return try {
            val values = ContentValues().apply {
                put(MediaStore.Downloads.DISPLAY_NAME, name)
                put(MediaStore.Downloads.MIME_TYPE, mime)
                put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS)
            }
            val uri = contentResolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                ?: return "could not create the file"
            contentResolver.openOutputStream(uri)!!.use { it.write(data) }
            if (view) runOnUiThread {
                val show = Intent(Intent.ACTION_VIEW).apply {
                    setDataAndType(uri, mime)
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
                }
                try { startActivity(show) } catch (e: Exception) {
                    android.util.Log.w("agentbench", "nothing opens $mime", e)
                }
            }
            ""
        } catch (e: Exception) {
            e.message ?: "could not save"
        }
    }

    fun pickBusy(): Boolean = reading > 0

    fun takePicked(): Picked? = picked.poll()

    @Deprecated("NativeActivity has no result callback of its own")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != PICK) return
        val uris = ArrayList<Uri>()
        if (resultCode == RESULT_OK && data != null) {
            data.clipData?.let { clip -> for (i in 0 until clip.itemCount) uris.add(clip.getItemAt(i).uri) }
            data.data?.let { if (uris.isEmpty()) uris.add(it) }
        }
        if (uris.isEmpty()) {
            reading -= 1
            return
        }
        Thread {
            for (uri in uris) picked.add(read(uri))
            reading -= 1
        }.start()
    }

    private fun read(uri: Uri): Picked {
        val name = name(uri)
        return try {
            val out = ByteArrayOutputStream()
            val buf = ByteArray(64 * 1024)
            contentResolver.openInputStream(uri)!!.use { input ->
                while (true) {
                    val n = input.read(buf)
                    if (n < 0) break
                    out.write(buf, 0, n)
                    if (out.size() > MAX_BYTES) return Picked(name, null)
                }
            }
            Picked(name, out.toByteArray())
        } catch (e: Exception) {
            Picked(name, null)
        }
    }

    private fun name(uri: Uri): String {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0)?.let { return it }
        }
        return uri.lastPathSegment ?: "file"
    }

    private companion object {
        const val PICK = 4711
        const val MAX_BYTES = 20 * 1024 * 1024
    }
}
