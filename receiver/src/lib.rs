// src/lib.rs
//
// Корень крейта. Экспортирует все модули.
// На Android собирается как cdylib (JNI-библиотека).
// На десктопе используется как rlib бинарником main.rs.

pub mod backend;
pub mod network;
pub mod types;

// Android-инициализация логгера выполняется при загрузке библиотеки
#[cfg(target_os = "android")]
#[allow(non_snake_case)]
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn JNI_OnLoad(
    _vm: jni::JavaVM,
    _: *mut std::ffi::c_void,
) -> jni::sys::jint {
    // Перенаправляем log::* в Android logcat с тегом "StreamReceiver"
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("StreamReceiver"),
    );
    log::info!("Rust library loaded (JNI_OnLoad)");
    jni::sys::JNI_VERSION_1_6
}