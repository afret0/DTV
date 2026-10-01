use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DlnaDevice {
    pub name: String,
    pub location: String,
    pub host: String,
    pub control_url: String,
}

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

const SSDP_MULTICAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const DLNA_PORTS: &[u16] = &[49152, 49494, 5000, 8200, 38520, 2869, 9080];
const SSDP_DISCOVER_MSG: &str = "\
M-SEARCH * HTTP/1.1\r\n\
HOST: 239.255.255.250:1900\r\n\
MAN: \"ssdp:discover\"\r\n\
MX: 2\r\n\
ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
\r\n";

fn discover_ssdp(local_ip: Ipv4Addr) -> Vec<DlnaDevice> {
    let bind_addr = SocketAddrV4::new(local_ip, 0);
    let socket = match UdpSocket::bind(bind_addr) {
        Ok(s) => s,
        Err(e) => { eprintln!("[DLNA] bind {} failed: {}", bind_addr, e); return vec![]; }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = socket.set_broadcast(true);
    let dst = SocketAddr::V4(SocketAddrV4::new(SSDP_MULTICAST_IP, SSDP_PORT));
    if socket.send_to(SSDP_DISCOVER_MSG.as_bytes(), dst).is_err() { return vec![]; }
    let mut devices = HashMap::new();
    let mut buf = [0u8; 8192];
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((size, _)) => {
                let data = String::from_utf8_lossy(&buf[..size]);
                if data.contains("200 OK") {
                    if let Some(dev) = parse_ssdp_response(&data) {
                        devices.entry(dev.location.clone()).or_insert(dev);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    devices.into_values().collect()
}

async fn scan_subnet_for_dlna(ifaces: &[(String, IpAddr)]) -> Vec<DlnaDevice> {
    let mut devices = HashMap::new();
    for (_name, ip) in ifaces {
        if let IpAddr::V4(v4) = ip {
            if v4.is_loopback() || v4.is_link_local() { continue; }
            let octets = v4.octets();
            let base = format!("{}.{}.{}", octets[0], octets[1], octets[2]);
            let tasks: Vec<_> = (1..=254).map(|last| {
                let ip = format!("{}.{}", base, last);
                tokio::spawn(async move {
                    for &port in DLNA_PORTS {
                        let addr = format!("{}:{}", ip, port);
                        if tokio::time::timeout(Duration::from_millis(300), tokio::net::TcpStream::connect(&addr)).await.is_ok_and(|r| r.is_ok()) {
                            for &desc_port in DLNA_PORTS {
                                let desc_url = format!("http://{}:{}/description.xml", ip, desc_port);
                                if let Some(xml) = fetch_dlna_description(&desc_url).await {
                                    let name = extract_device_name(&xml, "DLNA");
                                    if let Some(cu) = extract_avtransport_url(&xml, &desc_url) {
                                        return Some(DlnaDevice { name, location: desc_url, host: ip, control_url: cu });
                                    }
                                }
                            }
                        }
                    }
                    None
                })
            }).collect();
            for t in tasks { if let Ok(Some(dev)) = t.await { devices.entry(dev.location.clone()).or_insert(dev); } }
        }
    }
    devices.into_values().collect()
}

fn parse_ssdp_response(data: &str) -> Option<DlnaDevice> {
    let headers: HashMap<String, String> = data.lines().skip(1)
        .filter_map(|line| {
            let mut p = line.splitn(2, ':');
            Some((p.next()?.trim().to_lowercase(), p.next()?.trim().to_string()))
        }).collect();
    let st = headers.get("st")?;
    if !st.contains("MediaRenderer") && !st.contains("ssdp:all") { return None; }
    let loc = headers.get("location")?.clone();
    let host = url::Url::parse(&loc).ok()?.host_str()?.to_string();
    Some(DlnaDevice { name: String::new(), location: loc, host, control_url: String::new() })
}

async fn fetch_dlna_description(location: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .danger_accept_invalid_certs(true).no_proxy().build().ok()?;
    client.get(location).send().await.ok()?.text().await.ok()
}

fn extract_device_name(xml: &str, fallback: &str) -> String {
    for pat in &[r#"<friendlyName[^>]*>([^<]*)</friendlyName>"#, r"<modelName>([^<]*)</modelName>"] {
        if let Ok(re) = regex::Regex::new(pat) {
            if let Some(c) = re.captures(xml) {
                let raw = c.get(1).unwrap().as_str().trim().to_string();
                if !raw.is_empty() { return raw.replace("&amp;","&").replace("&lt;","<").replace("&gt;",">").replace("&quot;","\""); }
            }
        }
    }
    fallback.to_string()
}

fn extract_avtransport_url(xml: &str, base_url: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?s)<serviceType>urn:schemas-upnp-org:service:AVTransport:\d</serviceType>.*?<controlURL>([^<]*)</controlURL>").ok()?;
    let path = re.captures(xml)?.get(1)?.as_str().to_string();
    url::Url::parse(base_url).ok()?.join(&path).ok().map(|u| u.to_string())
}

#[tauri::command]
pub async fn discover_dlna_devices() -> Result<Vec<DlnaDevice>, String> {
    let ifaces = local_ip_address::list_afinet_netifas()
        .map_err(|e| format!("无法获取网络接口: {}", e))?;
    let local_ips: Vec<Ipv4Addr> = ifaces.iter()
        .filter_map(|(_n, ip)| if let IpAddr::V4(v4)=ip { if !v4.is_loopback()&&!v4.is_link_local(){Some(*v4)}else{None} } else { None }).collect();
    if local_ips.is_empty() { return Err("未检测到可用网络".to_string()); }
    let (ssdp_devs, scan_devs) = tokio::join!(
        async {
            let mut m = HashMap::new();
            for ip in &local_ips { for d in discover_ssdp(*ip) { m.entry(d.location.clone()).or_insert(d); } }
            m.into_values().collect::<Vec<_>>()
        },
        scan_subnet_for_dlna(&ifaces)
    );
    let mut candidates: HashMap<String, DlnaDevice> = HashMap::new();
    for d in ssdp_devs { candidates.entry(d.location.clone()).or_insert(d); }
    for d in scan_devs { candidates.entry(d.location.clone()).or_insert(d); }
    if candidates.is_empty() { return Err("未发现DLNA设备".to_string()); }
    let tasks: Vec<_> = candidates.into_values().map(|dev| {
        let dev_name = dev.name.clone();
        let dev_host = dev.host.clone();
        let dev_location = dev.location.clone();
        tokio::spawn(async move {
            if let Some(xml) = fetch_dlna_description(&dev_location).await {
                let name = extract_device_name(&xml, &dev_name);
                if let Some(cu) = extract_avtransport_url(&xml, &dev_location) {
                    return Some(DlnaDevice { name: if name.is_empty(){dev_host.clone()}else{name}, location: dev_location, host: dev_host, control_url: cu });
                }
            }
            None
        })
    }).collect();
    let mut resolved = vec![];
    for t in tasks { if let Ok(Some(d)) = t.await { resolved.push(d); } }
    Ok(resolved)
}

#[tauri::command]
pub async fn push_to_dlna(device_location: &str, stream_url: &str) -> Result<(), String> {
    let xml = fetch_dlna_description(device_location).await.ok_or("无法获取设备XML")?;
    let control_url = extract_avtransport_url(&xml, device_location).ok_or("设备不支持AVTransport")?;
    let metadata = format!(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="dtv-live" parentID="0" restricted="1"><dc:title>DTV 直播</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/x-flv:*">{}</res></item></DIDL-Lite>"#, stream_url);
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).no_proxy().build().map_err(|e| format!("HTTP: {}", e))?;
    let soap = format!(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#, stream_url, html_escape::encode_safe(&metadata));
    client.post(&control_url).header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"").header("Content-Type", "text/xml; charset=utf-8").body(soap).send().await.map_err(|e| format!("连接失败: {}", e))?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.post(&control_url).header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#Play\"").header("Content-Type", "text/xml; charset=utf-8").body(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"#).send().await.map_err(|e| format!("Play失败: {}", e))?;
    Ok(())
}
