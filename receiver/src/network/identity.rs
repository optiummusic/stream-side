use super::*;

pub(crate) async fn send_identity(mut send: SendStream) {
    let identify = make_client_identity();
    let bytes = postcard::to_stdvec(&identify).unwrap();
    if let Err(e) = send.write_all(&(bytes.len() as u32).to_le_bytes()).await {
        log::error!("write len failed: {e}");
        return;
    }

    if let Err(e) = send.write_all(&bytes).await {
        log::error!("write body failed: {e}");
        return;
    }
    let _ = send.finish();
}

fn make_client_identity() -> ControlPacket {
    let os = if cfg!(target_os = "android") {
        "Android"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else {
        "Unknown"
    }
    .to_string();

    let (model, name) = if cfg!(target_os = "android") {
        android_identity()
    } else if cfg!(target_os = "windows") {
        windows_identity()
    } else if cfg!(target_os = "macos") {
        macos_identity()
    } else if cfg!(target_os = "linux") {
        linux_identity()
    } else {
        unknown_identity()
    };

    // Если ControlPacket пока содержит только model/os,
    // склеиваем model + name в одно поле.
    let model = if name.is_empty() {
        model
    } else {
        format!("{model} ({name})")
    };

    ControlPacket::Identify { model, os }
}

fn android_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .or_else(|_| std::env::var("ANDROID_MODEL"))
        .unwrap_or_else(|_| "Android device".to_string());

    let name = std::env::var("CLIENT_NAME")
        .or_else(|_| std::env::var("ANDROID_DEVICE_NAME"))
        .unwrap_or_else(|_| "Android".to_string());

    (model, name)
}

fn windows_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| format!("{} {}", std::env::consts::ARCH, "Windows"));

    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Windows-PC".to_string());

    (model, name)
}

fn macos_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| "Mac".to_string());

    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Mac".to_string());

    (model, name)
}

fn linux_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL")
        .unwrap_or_else(|_| format!("Linux {}", std::env::consts::ARCH));

    let name = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("CLIENT_NAME"))
        .unwrap_or_else(|_| "Linux-host".to_string());

    (model, name)
}

fn unknown_identity() -> (String, String) {
    let model = std::env::var("CLIENT_MODEL").unwrap_or_else(|_| "Unknown device".to_string());
    let name = std::env::var("CLIENT_NAME").unwrap_or_else(|_| "Unknown".to_string());
    (model, name)
}