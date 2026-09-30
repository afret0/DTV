use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct CastInfo {
    pub stream_url: String,
    pub local_ip: String,
    pub lan_url: String,
}

#[cfg(not(target_os = "android"))]
use crate::StreamUrlStore;
#[cfg(not(target_os = "android"))]
use tauri::State;

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn get_cast_info(
    stream_url_store: State<'_, StreamUrlStore>,
) -> Result<CastInfo, String> {
    let stream_url = stream_url_store.url.lock().unwrap().clone();
    if stream_url.is_empty() {
        return Err("没有正在播放的直播流".to_string());
    }
    let ip = local_ip_address::local_ip().map_err(|e| format!("无法获取本机IP: {}", e))?;
    let lan_url = format!("http://{}:34719/live.flv", ip);
    Ok(CastInfo { stream_url, local_ip: ip.to_string(), lan_url })
}

#[cfg(target_os = "android")]
#[tauri::command]
pub async fn get_cast_info() -> Result<CastInfo, String> {
    Err("投屏功能在移动端不可用".to_string())
}
