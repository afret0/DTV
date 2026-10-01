use serde::Serialize;
use std::net::IpAddr;
use tauri::State;

use crate::StreamUrlStore;

/// The IP a renderer on the LAN must dial back to. `local_ip()` alone can pick
/// a cellular/VPN interface on phones, which makes casting fail silently (the
/// TV connects to an unreachable address), so private Wi-Fi ranges win and
/// tunnel/cellular interfaces are skipped.
pub fn best_lan_ip() -> Option<IpAddr> {
    let ifaces = local_ip_address::list_afinet_netifas().ok()?;
    let mut fallback: Option<IpAddr> = None;
    for (name, ip) in &ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() { continue; }
        let n = name.to_lowercase();
        if n.starts_with("rmnet") || n.starts_with("ccmni") || n.starts_with("tun")
            || n.starts_with("utun") || n.starts_with("tap") || n.starts_with("lo") { continue; }
        let o = v4.octets();
        let private = o[0] == 10
            || (o[0] == 192 && o[1] == 168)
            || (o[0] == 172 && (16..=31).contains(&o[1]));
        if private {
            println!("[cast] lan ip {} via {}", v4, n);
            return Some(IpAddr::V4(*v4));
        }
        if fallback.is_none() { fallback = Some(IpAddr::V4(*v4)); }
    }
    fallback.or_else(|| local_ip_address::local_ip().ok())
}

/// Proxy port every renderer is pointed at (must match `proxy::start_proxy_inner`).
pub const CAST_PORT: u16 = 34719;


#[derive(Serialize, Clone)]
pub struct CastInfo {
    pub stream_url: String,
    pub local_ip: String,
    pub lan_url: String,
}

#[tauri::command]
pub async fn get_cast_info(
    stream_url_store: State<'_, StreamUrlStore>,
) -> Result<CastInfo, String> {
    let stream_url = stream_url_store.url.lock().unwrap().clone();
    if stream_url.is_empty() {
        return Err("没有正在播放的直播流".to_string());
    }
    let ip = best_lan_ip().ok_or_else(|| "无法获取本机IP".to_string())?;
    let lan_url = format!("http://{}:{}/live.flv", ip, CAST_PORT);
    println!("[cast] cast info: local_ip={} lan_url={}", ip, lan_url);
    Ok(CastInfo {
        stream_url,
        local_ip: ip.to_string(),
        lan_url,
    })
}


#[cfg(test)]
mod cast_tests {
    use super::*;

    /// The cast URL must be dialable from the LAN: never loopback, always the
    /// shared proxy port. (On a LAN-less machine no IP is expected.)
    #[test]
    fn cast_ip_is_lan_routable() {
        match best_lan_ip() {
            Some(IpAddr::V4(v4)) => {
                assert!(!v4.is_loopback(), "loopback address cannot be cast to: {}", v4);
                assert!(!v4.is_unspecified(), "unspecified address cannot be cast to");
                println!("[cast test] would advertise {}", v4);
            }
            Some(IpAddr::V6(v6)) => assert!(!v6.is_loopback(), "loopback v6 cannot be cast to"),
            None => println!("[cast test] no LAN interface available"),
        }
        assert_eq!(CAST_PORT, 34719, "cast port must match the proxy");
    }
}
