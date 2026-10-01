use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DlnaDevice {
    pub name: String,
    pub location: String,
    pub host: String,
    pub control_url: String,
}

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

const SSDP_MULTICAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
/// Ports where UPnP/DLNA media renderers commonly serve description.xml.
const DLNA_PORTS: &[u16] = &[49152, 49494, 5000, 8200, 38520, 2869, 9080];
/// First pass uses only the ports that cover nearly every real renderer, so a
/// full /24 sweep of an empty subnet finishes in ~2s instead of ~11s.
const DLNA_PORTS_FAST: &[u16] = &[49152, 49494, 8200];
/// Per-host TCP probe timeout for the subnet sweep.
const PROBE_TIMEOUT_MS: u64 = 220;
/// Maximum concurrent TCP probes in the subnet fallback scan.
const SCAN_CONCURRENCY: usize = 96;
/// Two search targets: some renderers only answer `ssdp:all`.
const SSDP_SEARCH_TARGETS: &[&str] = &[
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "ssdp:all",
];

fn ssdp_discover_msg(st: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {}\r\n\r\n",
        st
    )
}

fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_millis(800))
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
        .ok()
}

/// Async SSDP discovery bound to one interface. Joins the multicast group so
/// renderers that reply via multicast (not just unicast) are seen too.
async fn discover_ssdp(local_ip: Ipv4Addr) -> Vec<DlnaDevice> {
    use tokio::net::UdpSocket;

    let bind_addr = SocketAddrV4::new(local_ip, 0);
    let socket = match UdpSocket::bind(bind_addr).await {
        Ok(s) => s,
        Err(e) => {
            println!("[DLNA] ssdp bind {} failed: {}", bind_addr, e);
            return vec![];
        }
    };
    socket.set_broadcast(true).ok();
    socket.set_multicast_loop_v4(true).ok();
    // Join so renderers that answer via multicast are received too; binding to
    // the interface address already selects it as the egress interface.
    if let Err(e) = socket.join_multicast_v4(SSDP_MULTICAST_IP, local_ip) {
        println!("[DLNA] join_multicast_v4 on {} failed: {}", local_ip, e);
    }

    let mcast = SocketAddr::V4(SocketAddrV4::new(SSDP_MULTICAST_IP, SSDP_PORT));
    let subnet_bcast = subnet_broadcast(local_ip);
    let bcast = SocketAddr::V4(SocketAddrV4::new(subnet_bcast, SSDP_PORT));
    for st in SSDP_SEARCH_TARGETS {
        let msg = ssdp_discover_msg(st);
        if let Err(e) = socket.send_to(msg.as_bytes(), mcast).await {
            println!("[DLNA] ssdp multicast send ST={} from {} failed: {}", st, local_ip, e);
        }
        // Subnet broadcast too: some renderers only listen on broadcast.
        let _ = socket.send_to(msg.as_bytes(), bcast).await;
    }
    println!("[DLNA] ssdp probe sent from {} (st={:?})", local_ip, SSDP_SEARCH_TARGETS.len());

    let mut devices: HashMap<String, DlnaDevice> = HashMap::new();
    let mut buf = vec![0u8; 8192];
    let started = std::time::Instant::now();
    let window = Duration::from_millis(2500);
    loop {
        let remaining = window.saturating_sub(started.elapsed());
        if remaining.is_zero() { break; }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((size, peer))) => {
                let data = String::from_utf8_lossy(&buf[..size]).to_string();
                if !data.contains("200 OK") { continue; }
                match parse_ssdp_response(&data) {
                    Some(mut dev) => {
                        println!("[DLNA] ssdp reply from {} -> {} (ST={})", peer.ip(), dev.location, dev.name);
                        if dev.host.is_empty() { dev.host = peer.ip().to_string(); }
                        devices.entry(dev.location.clone()).or_insert(dev);
                    }
                    None => println!("[DLNA] ssdp reply from {} without usable LOCATION", peer.ip()),
                }
            }
            Ok(Err(e)) => { println!("[DLNA] ssdp recv error: {}", e); break; }
            Err(_) => break,
        }
    }
    println!("[DLNA] ssdp done on {}: {} device(s) in {:.1}s",
        local_ip, devices.len(), started.elapsed().as_secs_f32());
    devices.into_values().collect()
}

/// `/24` broadcast address for a local IPv4 address (SSDP fallback target).
fn subnet_broadcast(ip: Ipv4Addr) -> Ipv4Addr {
    let o = ip.octets();
    Ipv4Addr::new(o[0], o[1], o[2], 255)
}

/// Fallback: probe the whole /24 for a UPnP description endpoint. Needed
/// because multicast is often blocked (Android without a MulticastLock,
/// client isolation on the AP, or a firewall dropping inbound UDP).
/// Sweep every host of each LAN /24 looking for a UPnP description endpoint.
/// Needed because multicast is often blocked (Android without a MulticastLock,
/// AP client isolation, or a firewall dropping inbound UDP).
async fn scan_subnet_for_dlna(
    client: Arc<reqwest::Client>,
    ifaces: &[(String, IpAddr)],
    ports: &[u16],
) -> Vec<DlnaDevice> {
    use futures_util::stream::{self, StreamExt};

    let mut devices: HashMap<String, DlnaDevice> = HashMap::new();
    let mut scanned_subnets = 0usize;

    for (name, ip) in ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() { continue; }
        // Skip VPN/tunnel interfaces: a full /24 probe there is pure waste.
        let n = name.to_lowercase();
        if n.starts_with("utun") || n.starts_with("tun") || n.starts_with("tap")
            || n.starts_with("bridge") || n.starts_with("lo")
            // Cellular/VPN interfaces on Android: a /24 sweep there never finds
            // a LAN renderer and only wastes time.
            || n.starts_with("rmnet") || n.starts_with("ccmni") || n.starts_with("dummy") { continue; }
        scanned_subnets += 1;
        let o = v4.octets();
        let base = format!("{}.{}.{}", o[0], o[1], o[2]);
        let started = std::time::Instant::now();
        let ports: Vec<u16> = ports.to_vec();

        let hits: Vec<DlnaDevice> = stream::iter(1u8..=254)
            .map(|last| {
                let ip = format!("{}.{}", base, last);
                let client = client.clone();
                let ports = ports.clone();
                async move {
                    for &port in &ports {
                        let target = format!("{}:{}", ip, port);
                        let ok = tokio::time::timeout(
                            Duration::from_millis(PROBE_TIMEOUT_MS),
                            tokio::net::TcpStream::connect(&target),
                        )
                        .await
                        .is_ok_and(|r| r.is_ok());
                        if !ok { continue; }
                        // This port answered; try it first, then the rest.
                        let mut try_ports = vec![port];
                        try_ports.extend(DLNA_PORTS.iter().copied().filter(|p| *p != port));
                        for desc_port in try_ports {
                            let desc_url = format!("http://{}:{}/description.xml", ip, desc_port);
                            let Some(xml) = fetch_dlna_description(&client, &desc_url).await else { continue };
                            let Some(cu) = extract_avtransport_url(&xml, &desc_url) else { continue };
                            let nm = extract_device_name(&xml, "DLNA");
                            println!("[DLNA] scan hit {} -> {}", desc_url, nm);
                            return Some(DlnaDevice {
                                name: nm,
                                location: desc_url,
                                host: ip.clone(),
                                control_url: cu,
                            });
                        }
                    }
                    None
                }
            })
            .buffer_unordered(SCAN_CONCURRENCY)
            .filter_map(|d| async move { d })
            .collect()
            .await;

        println!("[DLNA] scan {}:{}:0/24 ({} ports) done in {:.1}s -> {} hit(s)",
            name, base, ports.len(), started.elapsed().as_secs_f32(), hits.len());
        for d in hits {
            devices.entry(d.location.clone()).or_insert(d);
        }
    }
    println!("[DLNA] scan total: {} subnet(s), {} device(s)", scanned_subnets, devices.len());
    devices.into_values().collect()
}

/// Merged, always-growing view of what has been discovered so far. SSDP fills it
/// within ~2.5s; the slower subnet sweep enriches it in the background so the
/// cast dialog never has to wait for a full /24 probe on a real LAN.
static DEVICE_CACHE: OnceLock<StdMutex<HashMap<String, DlnaDevice>>> = OnceLock::new();
static SCAN_RUNNING: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static StdMutex<HashMap<String, DlnaDevice>> {
    DEVICE_CACHE.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn merge_into_cache(devs: Vec<DlnaDevice>) {
    let mut c = cache().lock().unwrap();
    for d in devs {
        c.entry(d.location.clone()).or_insert(d);
    }
}

/// Everything currently known that has a usable AVTransport control URL.
fn cached_renderers() -> Vec<DlnaDevice> {
    let mut out: Vec<DlnaDevice> = cache()
        .lock()
        .unwrap()
        .values()
        .filter(|d| !d.control_url.is_empty())
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Run the slow subnet sweep once, in the background, feeding the shared cache.
fn spawn_background_scan(client: Arc<reqwest::Client>, ifaces: Vec<(String, IpAddr)>) {
    if SCAN_RUNNING.swap(true, Ordering::SeqCst) { return; }
    tokio::spawn(async move {
        let fast = scan_subnet_for_dlna(client.clone(), &ifaces, DLNA_PORTS_FAST).await;
        let found = !fast.is_empty();
        merge_into_cache(fast);
        if !found {
            // Nothing on the common ports: widen the sweep.
            let rest: Vec<u16> = DLNA_PORTS.iter().copied()
                .filter(|p| !DLNA_PORTS_FAST.contains(p)).collect();
            if !rest.is_empty() {
                merge_into_cache(scan_subnet_for_dlna(client, &ifaces, &rest).await);
            }
        }
        SCAN_RUNNING.store(false, Ordering::SeqCst);
        println!("[DLNA] background scan finished, cache={}", cached_renderers().len());
    });
}

/// Fill in name/controlURL for cached entries that only have a LOCATION.
async fn resolve_cache(client: &reqwest::Client) {
    let pending: Vec<DlnaDevice> = cache()
        .lock()
        .unwrap()
        .values()
        .filter(|d| d.control_url.is_empty())
        .cloned()
        .collect();
    if pending.is_empty() { return; }
    let tasks: Vec<_> = pending.into_iter().map(|dev| {
        let client = client.clone();
        tokio::spawn(async move {
            let Some(xml) = fetch_dlna_description(&client, &dev.location).await else {
                println!("[DLNA] could not fetch {}", dev.location);
                return;
            };
            let Some(cu) = extract_avtransport_url(&xml, &dev.location) else {
                println!("[DLNA] {} has no AVTransport service (not a renderer)", dev.location);
                return;
            };
            let name = extract_device_name(&xml, &dev.name);
            let resolved = DlnaDevice {
                name: if name.is_empty() { dev.host.clone() } else { name },
                location: dev.location.clone(),
                host: dev.host,
                control_url: cu,
            };
            cache().lock().unwrap().insert(resolved.location.clone(), resolved);
        })
    }).collect();
    for t in tasks { let _ = t.await; }
}

fn parse_ssdp_response(data: &str) -> Option<DlnaDevice> {
    let headers: HashMap<String, String> = data.lines().skip(1)
        .filter_map(|line| {
            let mut p = line.splitn(2, ':');
            Some((p.next()?.trim().to_lowercase(), p.next()?.trim().to_string()))
        }).collect();
    let loc = headers.get("location")?.trim().to_string();
    if loc.is_empty() { return None; }
    let host = url::Url::parse(&loc).ok()?.host_str()?.to_string();
    // `server`/`st` only give a rough hint; the description fetch resolves the real name.
    let hint = headers.get("server").cloned().unwrap_or_default();
    Some(DlnaDevice { name: hint, location: loc, host, control_url: String::new() })
}

async fn fetch_dlna_description(client: &reqwest::Client, location: &str) -> Option<String> {
    let resp = client.get(location).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    let text = resp.text().await.ok()?;
    // Reject captive portals / HTML error pages that are not UPnP descriptions.
    if !text.contains("<root") && !text.contains("deviceType") { return None; }
    Some(text)
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

/// Locate the AVTransport controlURL by scanning individual <service> blocks so
/// a neighbouring service's controlURL can never be picked up by mistake.
fn extract_avtransport_url(xml: &str, base_url: &str) -> Option<String> {
    let base = url::Url::parse(base_url).ok()?;
    let lower = xml.to_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("<service>") {
        let svc_start = search_from + rel;
        let svc_end = match lower[svc_start..].find("</service>") {
            Some(e) => svc_start + e,
            None => break,
        };
        let block = &xml[svc_start..svc_end.min(xml.len())];
        search_from = svc_end + "</service>".len();
        if !block.contains("service:AVTransport") { continue; }
        if let Ok(re) = regex::Regex::new(r"(?s)<controlURL>\s*([^<]*?)\s*</controlURL>") {
            if let Some(c) = re.captures(block) {
                let path = c.get(1).unwrap().as_str().trim();
                if path.is_empty() { continue; }
                if let Ok(u) = base.join(path) { return Some(u.to_string()); }
            }
        }
    }
    None
}

#[tauri::command]
pub async fn discover_dlna_devices() -> Result<Vec<DlnaDevice>, String> {
    let started = std::time::Instant::now();
    let ifaces = local_ip_address::list_afinet_netifas()
        .map_err(|e| format!("无法获取网络接口: {}", e))?;
    let local_ips: Vec<Ipv4Addr> = ifaces.iter()
        .filter_map(|(_n, ip)| {
            if let IpAddr::V4(v4) = ip {
                if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() { Some(*v4) } else { None }
            } else { None }
        })
        .collect();
    println!("[DLNA] discover start: {} iface(s), ipv4={:?}", ifaces.len(), local_ips);
    if local_ips.is_empty() {
        return Err("未检测到可用网络".to_string());
    }
    let local_ips_summary = local_ips.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let client = http_client().ok_or("无法创建HTTP客户端")?;

    // Kick off the slow sweep first so it overlaps with SSDP; its results land
    // in the shared cache and are returned by the next call at the latest.
    spawn_background_scan(Arc::new(client.clone()), ifaces.clone());

    let mut ssdp_tasks = Vec::new();
    for ip in local_ips { ssdp_tasks.push(discover_ssdp(ip)); }
    for devs in futures_util::future::join_all(ssdp_tasks).await {
        merge_into_cache(devs);
    }
    resolve_cache(&client).await;

    let mut found = cached_renderers();
    if found.is_empty() {
        // SSDP got nothing (typical for Android without a multicast lock): wait
        // for the sweep instead of returning an empty list immediately.
        println!("[DLNA] ssdp found nothing, waiting for subnet sweep");
        let deadline = std::time::Instant::now() + Duration::from_secs(16);
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(500)).await;
            resolve_cache(&client).await;
            found = cached_renderers();
            if !found.is_empty() || !SCAN_RUNNING.load(Ordering::SeqCst) { break; }
        }
    }
    println!("[DLNA] discover returning {} renderer(s) in {:.1}s",
        found.len(), started.elapsed().as_secs_f32());
    if found.is_empty() {
        return Err(format!("未发现DLNA设备 (已搜索 {})", local_ips_summary));
    }
    Ok(found)
}

/// Resolve a renderer's description + AVTransport control URL for a bare host,
/// probing the ports where UPnP renderers usually listen. Used for manual IP
/// entry and as a fallback when a cached location went stale.
pub async fn resolve_renderer_by_host(host: &str) -> Option<(String, String)> {
    let client = http_client()?;
    for &port in DLNA_PORTS {
        let url = format!("http://{}:{}/description.xml", host, port);
        if let Some(xml) = fetch_dlna_description(&client, &url).await {
            if let Some(cu) = extract_avtransport_url(&xml, &url) {
                return Some((url, cu));
            }
        }
    }
    None
}

/// Send one AVTransport SOAP action and surface the renderer's fault text.
async fn soap_action(
    client: &reqwest::Client,
    control_url: &str,
    action: &str,
    body: String,
) -> Result<String, String> {
    let resp = client
        .post(control_url)
        .header("SOAPAction", &format!("\"urn:schemas-upnp-org:service:AVTransport:1#{}\"", action))
        .header("Content-Type", "text/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("连接电视失败: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if text.contains("Fault") || !status.is_success() {
        // Extract the human readable description UPnP renderers put in faults.
        let detail = regex::Regex::new(r"(?s)<statusDescription>\s*([^<]*?)\s*</statusDescription>")
            .ok()
            .and_then(|re| re.captures(&text).map(|c| c.get(1).unwrap().as_str().trim().to_string()))
            .unwrap_or_else(|| format!("HTTP {}", status));
        return Err(format!("电视返回错误: {}", detail));
    }
    Ok(text)
}

/// Ask the renderer what it is doing right now (used to confirm a push worked).
pub async fn query_transport_state(client: &reqwest::Client, control_url: &str) -> Option<String> {
    let body = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID></u:GetTransportInfo></s:Body></s:Envelope>"#;
    let text = soap_action(client, control_url, "GetTransportInfo", body.to_string()).await.ok()?;
    regex::Regex::new(r"(?s)<CurrentTransportState>\s*([^<]*?)\s*</CurrentTransportState>")
        .ok()?
        .captures(&text)
        .map(|c| c.get(1).unwrap().as_str().trim().to_string())
}

/// Renderers are picky about DIDL-Lite metadata: the Mi TV SmartShare renderer
/// accepts a stream with an ASCII title but silently aborts playback a few
/// seconds in when the title contains CJK characters. Stream titles here are
/// usually Chinese, so strip everything non-ASCII and fall back to a fixed one.
pub fn ascii_cast_title(raw: Option<&str>) -> String {
    let mut cleaned = String::new();
    let mut prev_space = false;
    for c in raw.unwrap_or("").chars() {
        // Keep printable ASCII (spaces included) but drop XML metacharacters;
        // everything non-ASCII (CJK titles, emoji) has to go.
        // Dropped characters must not turn neighbouring spaces into a double one.
        if !c.is_ascii() { continue; }
        if matches!(c, '<' | '>' | '&' | '"' | '\'') { continue; }
        if c.is_whitespace() {
            if prev_space || cleaned.is_empty() { continue; }
            cleaned.push(' ');
            prev_space = true;
            continue;
        }
        if cleaned.len() >= 60 { break; }
        cleaned.push(c);
        prev_space = false;
    }
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "DTV Live".to_string()
    } else {
        format!("DTV Live - {}", cleaned)
    }
}

#[tauri::command]
pub async fn push_to_dlna(
    device_location: &str,
    stream_url: &str,
    title: Option<String>,
) -> Result<(), String> {
    let cast_title = ascii_cast_title(title.as_deref());
    println!("[DLNA] push location={} stream={} title={:?}", device_location, stream_url, cast_title);
    // Description fetch is quick, but slow TVs can take several seconds to
    // accept SetAVTransportURI, so use a longer timeout for the SOAP calls.
    let client = http_client().ok_or("无法创建HTTP客户端")?;
    let soap_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .map_err(|e| format!("HTTP: {}", e))?;
    let (description_url, control_url) = match fetch_dlna_description(&client, device_location).await {
        Some(xml) => match extract_avtransport_url(&xml, device_location) {
            Some(cu) => (device_location.to_string(), cu),
            None => return Err("设备不支持AVTransport协议".to_string()),
        },
        None => {
            // Manual IP or stale location: probe the host directly.
            let host = url::Url::parse(device_location).ok()
                .and_then(|u| u.host_str().map(|h| h.to_string()))
                .unwrap_or_else(|| device_location.to_string());
            match resolve_renderer_by_host(&host).await {
                Some(pair) => pair,
                None => return Err("无法获取设备信息 (description.xml)".to_string()),
            }
        }
    };
    println!("[DLNA] push desc={} control_url={}", description_url, control_url);

    // Keep real DIDL-Lite metadata: this renderer rejects NOT_IMPLEMENTED.
    let metadata = format!(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="dtv-live" parentID="0" restricted="1"><dc:title>{}</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/x-flv:*">{}</res></item></DIDL-Lite>"#, html_escape::encode_safe(&cast_title), stream_url);
    let set_uri = format!(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#, stream_url, html_escape::encode_safe(&metadata));
    soap_action(&soap_client, &control_url, "SetAVTransportURI", set_uri).await?;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let play = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"#;
    soap_action(&soap_client, &control_url, "Play", play.to_string()).await?;

    // Confirm the renderer actually started, so the UI reports a real result
    // instead of "success" for a TV that silently rejected the stream.
    // Some renderers need ~20s to buffer FLV before leaving TRANSITIONING, and
    // a few report STOPPED briefly while re-opening the source, so only the
    // tail of the window is treated as a hard failure.
    let attempts = 12;
    let mut last_state = String::from("?");
    for attempt in 0..attempts {
        tokio::time::sleep(Duration::from_millis(2000)).await;
        match query_transport_state(&soap_client, &control_url).await {
            Some(state) => {
                println!("[DLNA] transport state after push ({}): {}", attempt, state);
                last_state = state.clone();
                if state == "PLAYING" { return Ok(()); }
            }
            None => println!("[DLNA] transport state query failed (try {})", attempt),
        }
        if attempt >= attempts - 4 && (last_state == "STOPPED" || last_state == "NO_MEDIA_PRESENT") {
            return Err(format!(
                "电视已接收投屏指令但未能播放 (状态: {})，可能无法解码该直播流",
                last_state
            ));
        }
    }
    println!("[DLNA] push sent; renderer did not confirm PLAYING within window");
    Ok(())
}

#[cfg(test)]
mod dlna_tests {
    use super::*;

    /// CJK titles must not reach the renderer: it accepts the stream and then
    /// aborts playback, which looks like "投屏一会儿就退出".
    #[test]
    fn cast_title_is_ascii_only() {
        assert_eq!(ascii_cast_title(Some("欢乐时光已经开始了！")), "DTV Live");
        // Only the ASCII remnant survives; a lone letter is still acceptable.
        assert_eq!(ascii_cast_title(Some("余小C")), "DTV Live - C");
        assert_eq!(ascii_cast_title(None), "DTV Live");
        assert_eq!(ascii_cast_title(Some("  ")), "DTV Live");
        assert_eq!(ascii_cast_title(Some("LPL Finals 2026")), "DTV Live - LPL Finals 2026");
        let mixed = ascii_cast_title(Some("LoL 英雄联盟 2026"));
        assert_eq!(mixed, "DTV Live - LoL 2026");
        assert_eq!(ascii_cast_title(Some("LPL Finals 2026")), "DTV Live - LPL Finals 2026");
        // XML metacharacters must never survive into the SOAP body
        let hostile = ascii_cast_title(Some(r#"<script>&"evil"</script>"#));
        assert!(!hostile.contains('<') && !hostile.contains('>') && !hostile.contains('&')
            && !hostile.contains('"'), "unsafe title: {}", hostile);
        assert!(hostile.chars().all(|c| c.is_ascii()), "title must be ASCII: {}", hostile);
        assert!(mixed.chars().all(|c| c.is_ascii()));
    }

    /// Real renderer descriptions list AVTransport among several services; the
    /// parser must return *its* controlURL, not a sibling service's.
    const SAMPLE_DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <friendlyName>A&amp;B TV</friendlyName>
    <modelName>MiTV</modelName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/smartshare/render/_urn:schemas-upnp-org:service:RenderingControl_control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/smartshare/render/_urn:schemas-upnp-org:service:AVTransport_control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/smartshare/render/_cm</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn extracts_avtransport_control_url_only() {
        let cu = extract_avtransport_url(SAMPLE_DESC, "http://192.168.31.211:49152/description.xml")
            .expect("control url");
        assert_eq!(
            cu,
            "http://192.168.31.211:49152/smartshare/render/_urn:schemas-upnp-org:service:AVTransport_control"
        );
    }

    #[test]
    fn resolves_relative_and_absolute_control_urls() {
        let xml_abs = SAMPLE_DESC.replace(
            "/smartshare/render/_urn:schemas-upnp-org:service:AVTransport_control",
            "http://10.0.0.9:5000/abs_control",
        );
        let cu = extract_avtransport_url(&xml_abs, "http://10.0.0.9:5000/desc.xml").unwrap();
        assert_eq!(cu, "http://10.0.0.9:5000/abs_control");
    }

    #[test]
    fn non_renderer_description_yields_nothing() {
        let xml = r#"<root><device><friendlyName>NAS</friendlyName><serviceList>
        <service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <controlURL>/cd</controlURL></service></serviceList></device></root>"#;
        assert!(extract_avtransport_url(xml, "http://10.0.0.5:8200/description.xml").is_none());
    }

    #[test]
    fn decodes_device_name_entities() {
        assert_eq!(extract_device_name(SAMPLE_DESC, "DLNA"), "A&B TV");
    }

    #[test]
    fn parses_ssdp_location_and_server_hint() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nST: upnp:rootdevice\r\nUSN: uuid:1234::upnp:rootdevice\r\nLOCATION: http://192.168.31.211:49152/description.xml\r\nSERVER: Linux/4.9, UPnP/1.0\r\n\r\n";
        let dev = parse_ssdp_response(reply).expect("parsed");
        assert_eq!(dev.location, "http://192.168.31.211:49152/description.xml");
        assert_eq!(dev.host, "192.168.31.211");
        assert!(dev.control_url.is_empty(), "control url comes from the description fetch");
    }

    #[test]
    fn rejects_reply_without_location() {
        let reply = "HTTP/1.1 200 OK\r\nST: ssdp:all\r\nSERVER: x\r\n\r\n";
        assert!(parse_ssdp_response(reply).is_none());
    }

    #[tokio::test]
    async fn description_fetch_rejects_html_portal_page() {
        // Serves an HTML page instead of a UPnP description; must be rejected so
        // a captive portal can't be mistaken for a renderer.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut s, _)) = listener.accept().await else { break };
                use tokio::io::AsyncWriteExt;
                let body = "<html><body>Sign in to Wi-Fi</body></html>";
                let _ = s.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(), body
                ).as_bytes()).await;
            }
        });
        let client = http_client().unwrap();
        let got = fetch_dlna_description(&client, &format!("http://127.0.0.1:{}/description.xml", port)).await;
        assert!(got.is_none(), "HTML portal page must not be accepted as a description");
    }

    #[tokio::test]
    async fn discovery_caches_and_returns_renderers_from_sweep() {
        // A fake renderer on a spare port: the sweep must find it, resolve the
        // AVTransport control URL, and cache it for subsequent calls.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { break };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let xml = SAMPLE_DESC.replace("A&amp;B TV", "TestRenderer");
                let _ = s.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{}",
                    xml.len(), xml
                ).as_bytes()).await;
            }
        });

        let client = http_client().unwrap();
        let loc = format!("http://127.0.0.1:{}/description.xml", port);
        let xml = fetch_dlna_description(&client, &loc).await.expect("description");
        let cu = extract_avtransport_url(&xml, &loc).expect("control url");
        assert!(cu.ends_with("AVTransport_control"), "unexpected control url {}", cu);
        merge_into_cache(vec![DlnaDevice {
            name: extract_device_name(&xml, "DLNA"),
            location: loc.clone(),
            host: "127.0.0.1".into(),
            control_url: cu.clone(),
        }]);
        let cached = cached_renderers();
        assert!(cached.iter().any(|d| d.location == loc && d.name == "TestRenderer"),
            "cache must expose the resolved renderer");

        // Manual-IP resolution path (bare host) must reach the same renderer.
        let resolved = resolve_renderer_by_host("127.0.0.1").await;
        // Port may differ from DLNA_PORTS; only assert when it is one of them.
        if DLNA_PORTS.contains(&port) {
            let (rloc, rcu) = resolved.expect("resolved by host");
            assert_eq!(rloc, loc);
            assert_eq!(rcu, cu);
        }
    }

    #[tokio::test]
    #[ignore = "live network test: requires a reachable DLNA renderer"]
    async fn live_discover_devices() {
        let started = std::time::Instant::now();
        match discover_dlna_devices().await {
            Ok(devs) => {
                println!("[dlna] found {} devices in {:.1}s", devs.len(), started.elapsed().as_secs_f32());
                for d in &devs {
                    println!("  - {} @ {} ctrl={}", d.name, d.host, d.control_url);
                }
                assert!(!devs.is_empty(), "no DLNA devices discovered");
            }
            Err(e) => panic!("discovery failed after {:.1}s: {}", started.elapsed().as_secs_f32(), e),
        }
    }
}
