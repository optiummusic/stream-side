// android/app/src/main/java/com/example/streamreceiver/MainActivity.kt
package com.example.streamreceiver

import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import android.view.Choreographer

private const val STREAM_WIDTH  = 1920
private const val STREAM_HEIGHT = 1080

private class VsyncBridge {
    private var running = false

    private val choreographer by lazy { Choreographer.getInstance() }

    private val callback = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!running) return
            NativeLib.onVsync(frameTimeNanos)
            choreographer.postFrameCallback(this)
        }
    }

    fun start() {
        if (running) return
        running = true
        choreographer.postFrameCallback(callback)
    }

    fun stop() {
        if (!running) return
        running = false
        choreographer.removeFrameCallback(callback)
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        WindowCompat.getInsetsController(window, window.decorView).apply {
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            hide(WindowInsetsCompat.Type.systemBars())
        }
        window.attributes.layoutInDisplayCutoutMode =
            WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES

        setContent {
            MaterialTheme {
                StreamReceiverScreen()
            }
        }
    }
}

@Composable
fun StreamReceiverScreen() {
    // ── Состояние ────────────────────────────────────────────────────────────
    var host        by remember { mutableStateOf("") }
    var port        by remember { mutableStateOf("4433") }
    var isConnected by remember { mutableStateOf(false) }
    var statusText  by remember { mutableStateOf("") }
    var latencyStats by remember { mutableStateOf("") }
    val vsyncBridge = remember { VsyncBridge() }

    DisposableEffect(Unit) {
        vsyncBridge.start()
        onDispose {
            vsyncBridge.stop()
        }
    }
    // Автоскрывать статусную плашку через 3 секунды
    LaunchedEffect(statusText) {
        if (statusText.isNotEmpty()) {
            kotlinx.coroutines.delay(3_000)
            statusText = ""
        }
    }

    LaunchedEffect(isConnected) {
        while (isConnected) {
            latencyStats = NativeLib.getLatencyStats()
            kotlinx.coroutines.delay(500)
        }
        latencyStats = ""
    }

    Box(modifier = Modifier.fillMaxSize()) {

        // ── SurfaceView (всегда на заднем слое) ──────────────────────────────
        //
        // SurfaceView, а не TextureView: аппаратный кодек рендерит напрямую
        // в выделенный оконный слой, минуя GPU-композитинг. ~1-2 кадра меньше задержки.
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory  = { context ->
                SurfaceView(context).also { sv ->
                    sv.holder.addCallback(object : SurfaceHolder.Callback {

                        override fun surfaceCreated(holder: SurfaceHolder) {
                            // Инициализируем декодер — подключение запускается отдельно кнопкой
                            NativeLib.initBackend(holder.surface, STREAM_WIDTH, STREAM_HEIGHT)
                        }

                        override fun surfaceChanged(
                            holder: SurfaceHolder, format: Int, width: Int, height: Int,
                        ) {
                            // adaptive-playback=1 справится с изменением размера без перезапуска
                        }

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            // КРИТИЧНО: остановить ВСЁ до выхода из callback'а!
                            // После возврата платформа уничтожает Surface — UB если Rust ещё пишет в него.
                            NativeLib.shutdownBackend()
                            isConnected = false
                            statusText  = "Surface destroyed"
                        }
                    })
                }
            }
        )

        // ── Форма подключения (показывается когда не подключены) ─────────────
        if (!isConnected) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(Color.Black.copy(alpha = 0.55f)),
                contentAlignment = Alignment.Center,
            ) {
                Card(
                    modifier  = Modifier.width(320.dp),
                    elevation = CardDefaults.cardElevation(8.dp),
                ) {
                    Column(
                        modifier            = Modifier.padding(24.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Text(
                            text  = "Connecting..",
                            style = MaterialTheme.typography.titleMedium,
                        )

                        OutlinedTextField(
                            value         = host,
                            onValueChange = { host = it },
                            label         = { Text("IP адрес") },
                            placeholder   = { Text("") },
                            singleLine    = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                            modifier      = Modifier.fillMaxWidth(),
                        )

                        OutlinedTextField(
                            value         = port,
                            onValueChange = { v -> if (v.all { it.isDigit() } && v.length <= 5) port = v },
                            label         = { Text("Port") },
                            singleLine    = true,
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            modifier      = Modifier.fillMaxWidth(),
                        )

                        Button(
                            onClick  = {
                                val portInt = port.toIntOrNull() ?: 4433
                                NativeLib.startNetworking(host, portInt)
                                isConnected = true
                                statusText  = "Connecting to $host:$portInt..."
                            },
                            modifier = Modifier.fillMaxWidth(),
                            enabled  = host.isNotBlank() && port.isNotBlank(),
                        ) {
                            Text("Connect")
                        }
                    }
                }
            }
        }

        // ── Кнопка "Отключить" (показывается когда подключены) ───────────────
        if (isConnected) {
            Button(
                onClick  = {
                    NativeLib.stopNetworking()
                    isConnected = false
                    statusText  = "Disabled"
                },
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(top = 16.dp, end = 16.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.errorContainer,
                    contentColor   = MaterialTheme.colorScheme.onErrorContainer,
                ),
            ) {
                Text("Disable")
            }
        }

        if (latencyStats.isNotEmpty()) {
            Text(
                text = "Lat: $latencyStats",
                color = Color.Green,
                style = MaterialTheme.typography.labelSmall,
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .padding(top = 16.dp, start = 16.dp)
                    .background(Color.Black.copy(alpha = 0.6f))
                    .padding(horizontal = 6.dp, vertical = 2.dp)
            )
        }

        // ── Статусная плашка (снизу, автоскрывается через 3с) ────────────────
        if (statusText.isNotEmpty()) {
            Card(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 48.dp, start = 16.dp, end = 16.dp),
            ) {
                Text(
                    text     = statusText,
                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    style    = MaterialTheme.typography.bodySmall,
                )
            }
        }
    }
}