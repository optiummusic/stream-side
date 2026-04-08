// android/app/src/main/java/com/example/streamreceiver/NativeLib.kt
package com.example.streamreceiver

import android.view.Surface

/**
 * JNI-мост к Rust-библиотеке `libstream_receiver.so`.
 *
 * Порядок вызовов:
 *   1. [initBackend]     — после того как Surface создан (surfaceCreated)
 *   2. [startNetworking] — при нажатии кнопки "Подключить"
 *   3. [stopNetworking]  — при нажатии кнопки "Отключить"
 *   4. [shutdownBackend] — при уничтожении Surface (surfaceDestroyed)
 *
 * Примечание: [shutdownBackend] внутри вызывает [stopNetworking] автоматически,
 * поэтому перед ним явный вызов [stopNetworking] не обязателен.
 */
object NativeLib {

    init {
        System.loadLibrary("stream_receiver")
    }

    /**
     * Инициализировать аппаратный HEVC-декодер MediaCodec с переданным Surface.
     *
     * @param surface Android Surface из SurfaceView.
     * @param width   Ожидаемая ширина видеопотока.
     * @param height  Ожидаемая высота видеопотока.
     */
    external fun initBackend(surface: Surface, width: Int, height: Int)

    /**
     * Запустить QUIC-клиент для подключения к sender'у.
     *
     * Если вызывается повторно — автоматически останавливает предыдущее соединение.
     *
     * @param host IP-адрес sender'а, например "192.168.1.5"
     * @param port Порт sender'а, по умолчанию 4433
     */
    external fun startNetworking(host: String, port: Int)

    /**
     * Остановить QUIC-клиент и освободить tokio runtime.
     *
     * Безопасно вызывать даже если соединение не было установлено.
     */
    external fun stopNetworking()

    /**
     * Остановить декодер и освободить ANativeWindow.
     *
     * ОБЯЗАТЕЛЬНО вызывать из surfaceDestroyed ДО возврата из него.
     * Внутри также вызывает stopNetworking.
     */
    external fun shutdownBackend()

    /**
     * Получить текущую задержку (строку) от Rust-бекенда.
     */
    external fun getLatencyStats(): String

    /**
     * Колбек от отрисовки кадра для показа задержки 
     */
    external fun onVsync(frameTimeNanos: Long)
}