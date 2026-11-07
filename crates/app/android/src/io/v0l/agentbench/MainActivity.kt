package io.v0l.agentbench

import android.app.NativeActivity
import android.content.Intent
import android.content.ContentValues
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.content.Context
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.ExtractedText
import android.view.inputmethod.ExtractedTextRequest
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import java.io.ByteArrayOutputStream
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicReference

/** One file the user picked. `data` is null when it could not be read or is too big. */
class Picked(val name: String, val data: ByteArray?)

/** What the keyboard types into: NativeActivity gives it no editor, so Gboard would show no
 *  clipboard chip and send only key presses. It holds a copy of the focused field's text, which
 *  every keyboard edit changes; after each one the whole text and selection, `start end\ntext`
 *  in UTF-16 units converted to characters, is left for the Rust side to take, and `wake` is
 *  called so it draws a frame. The Rust side puts the field's own text back with `sync`. */
class ImeView(context: Context, private val wake: () -> Unit) : View(context) {
    private val text: Editable = Editable.Factory.getInstance().newEditable("")
    private val pending = AtomicReference<String?>(null)
    private var batch = 0
    private var last = ""

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onCheckIsTextEditor() = true

    fun take(): String? = pending.getAndSet(null)

    private fun imm() = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager

    private fun chars(at: Int): Int = Character.codePointCount(text, 0, at.coerceIn(0, text.length))

    private fun units(chars: Int): Int {
        val count = Character.codePointCount(text, 0, text.length)
        return Character.offsetByCodePoints(text, 0, chars.coerceIn(0, count))
    }

    private fun report() {
        val start = Selection.getSelectionStart(text).coerceAtLeast(0)
        val end = Selection.getSelectionEnd(text).coerceAtLeast(0)
        val a = minOf(start, end)
        val b = maxOf(start, end)
        imm().updateSelection(this, a, b, BaseInputConnection.getComposingSpanStart(text),
            BaseInputConnection.getComposingSpanEnd(text))
        val line = "${chars(a)} ${chars(b)}\n$text"
        if (line == last) return
        last = line
        pending.set(line)
        wake()
    }

    private fun changed() {
        if (batch == 0) report()
    }

    fun sync(line: String) {
        if (pending.get() != null) return
        val head = line.substringBefore('\n')
        val body = line.substringAfter('\n', "")
        val start = head.substringBefore(' ').toIntOrNull() ?: return
        val end = head.substringAfter(' ').toIntOrNull() ?: return
        val same = text.toString() == body
        if (!same) {
            BaseInputConnection.removeComposingSpans(text)
            text.replace(0, text.length, body)
        }
        Selection.setSelection(text, units(start), units(end))
        last = "$start $end\n$body"
        if (same) {
            imm().updateSelection(this, units(start), units(end), -1, -1)
        } else {
            imm().restartInput(this)
        }
    }

    private fun deleteSelectionOr(before: Int, after: Int) {
        val start = Selection.getSelectionStart(text).coerceAtLeast(0)
        val end = Selection.getSelectionEnd(text).coerceAtLeast(0)
        val a = minOf(start, end)
        val b = maxOf(start, end)
        if (a != b) {
            text.delete(a, b)
        } else if (before > 0 && a > 0) {
            text.delete(Character.offsetByCodePoints(text, a, -1), a)
        } else if (after > 0 && b < text.length) {
            text.delete(b, Character.offsetByCodePoints(text, b, 1))
        }
    }

    private fun insert(s: String) {
        val start = Selection.getSelectionStart(text).coerceAtLeast(0)
        val end = Selection.getSelectionEnd(text).coerceAtLeast(0)
        text.replace(minOf(start, end), maxOf(start, end), s)
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI
        outAttrs.initialSelStart = Selection.getSelectionStart(text)
        outAttrs.initialSelEnd = Selection.getSelectionEnd(text)
        if (Build.VERSION.SDK_INT >= 30) outAttrs.setInitialSurroundingText(text)
        return object : BaseInputConnection(this, true) {
            override fun getEditable(): Editable = text

            override fun beginBatchEdit(): Boolean {
                batch += 1
                return true
            }

            override fun endBatchEdit(): Boolean {
                if (batch > 0) batch -= 1
                if (batch == 0) report()
                return batch > 0
            }

            override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
                super.commitText(text, newCursorPosition)
                changed()
                return true
            }

            override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
                super.setComposingText(text, newCursorPosition)
                changed()
                return true
            }

            override fun setComposingRegion(start: Int, end: Int): Boolean {
                super.setComposingRegion(start, end)
                changed()
                return true
            }

            override fun finishComposingText(): Boolean {
                super.finishComposingText()
                changed()
                return true
            }

            override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
                super.deleteSurroundingText(beforeLength, afterLength)
                changed()
                return true
            }

            override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
                super.deleteSurroundingTextInCodePoints(beforeLength, afterLength)
                changed()
                return true
            }

            override fun setSelection(start: Int, end: Int): Boolean {
                super.setSelection(start, end)
                changed()
                return true
            }

            override fun getExtractedText(request: ExtractedTextRequest?, flags: Int): ExtractedText {
                return ExtractedText().also {
                    it.text = text.toString()
                    it.startOffset = 0
                    it.partialStartOffset = -1
                    it.partialEndOffset = -1
                    it.selectionStart = Selection.getSelectionStart(text)
                    it.selectionEnd = Selection.getSelectionEnd(text)
                }
            }

            override fun sendKeyEvent(event: KeyEvent): Boolean {
                if (event.action != KeyEvent.ACTION_DOWN) return true
                removeComposingSpans(text)
                when (event.keyCode) {
                    KeyEvent.KEYCODE_DEL -> deleteSelectionOr(1, 0)
                    KeyEvent.KEYCODE_FORWARD_DEL -> deleteSelectionOr(0, 1)
                    KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> insert("\n")
                    else -> {
                        val c = event.unicodeChar
                        if (c == 0) return this@ImeView.rootView.dispatchKeyEvent(event)
                        insert(String(Character.toChars(c)))
                    }
                }
                changed()
                return true
            }
        }
    }
}

/** The app's window. It keeps the newest intent, so tapping a notification while the app is open
 *  tells the Rust side which chat to show. Plain NativeActivity ignores new intents. It also
 *  opens the system file picker and hands the chosen files to the Rust side to take. */
class MainActivity : NativeActivity() {
    private val picked = ConcurrentLinkedQueue<Picked>()
    @Volatile private var reading = 0
    @Volatile private var imeView: ImeView? = null

    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        val view = ImeView(this) {
            try {
                imeWake()
            } catch (e: UnsatisfiedLinkError) {
            }
        }
        addContentView(view, ViewGroup.LayoutParams(1, 1))
        imeView = view
    }

    /** Registered from the Rust side once its frame loop runs. */
    external fun imeWake()

    fun takeIme(): String? = imeView?.take()

    fun syncIme(line: String) {
        runOnUiThread { imeView?.sync(line) }
    }

    fun wantTextInput(on: Boolean) {
        runOnUiThread {
            val view = imeView ?: return@runOnUiThread
            val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
            if (on) {
                view.requestFocus()
                imm.restartInput(view)
                imm.showSoftInput(view, 0)
            } else {
                imm.hideSoftInputFromWindow(view.windowToken, 0)
                view.clearFocus()
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
    }

    override fun onDestroy() {
        android.util.Log.i("agentbench", "activity destroyed, ending the window process")
        android.os.Process.killProcess(android.os.Process.myPid())
        super.onDestroy()
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
