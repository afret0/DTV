use actix_web::{dev::ServerHandle, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use futures_util::StreamExt;
use reqwest::Client;
use crate::StreamUrlStore;
use serde::Deserialize;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;
use std::time::Duration;
use tauri::{AppHandle, State};
use tokio::sync::mpsc;

#[derive(Default)]
pub struct ProxyServerHandle(pub Arc<StdMutex<Option<ServerHandle>>>);

pub type ServerHandleForStore = ServerHandle;

const HUYA_HYSDK_UA: &str =
    "HYSDK(Windows,30000002)_APP(pc_exe&7080000&official)_SDK(trans&2.34.0.5795)";

async fn find_free_port() -> u16 { 34719 }

fn build_upstream_request(client: &Client, url: &str) -> reqwest::RequestBuilder {
    let mut req = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .header("Accept", "video/x-flv,application/octet-stream,*/*")
        .header("Range", "bytes=0-")
        .header("Connection", "keep-alive");

    if url.contains("douyu") || url.contains("douyucdn") {
        req = req.header("Referer", "https://www.douyu.com/");
    }
    if url.contains("huya.com") || url.contains("hy-cdn.com") || url.contains("huyaimg.com") {
        req = req
            .header("User-Agent", HUYA_HYSDK_UA)
            .header("Referer", "https://www.huya.com/")
            .header("Origin", "https://www.huya.com");
    }
    if url.contains("bilivideo") || url.contains("bilibili.com") || url.contains("hdslb.com") {
        req = req.header("Referer", "https://live.bilibili.com/");
    }
    if url.contains("douyin") || url.contains("douyincdn") {
        req = req.header("Referer", "https://live.douyin.com/");
    }
    req
}

/// Keeps the Douyu signed URL in the store fresh (signatures expire in ~300s),
/// so reconnects after a CDN cut always use a valid URL.
static REFRESHER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn spawn_url_refresher(store: StreamUrlStore) {
    if REFRESHER_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("refresher runtime");
        rt.block_on(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(200)).await;
            if store.platform.lock().unwrap().as_deref() != Some("douyu") {
                continue;
            }
            let room = store.room_id.lock().unwrap().clone();
            let quality = store.quality.lock().unwrap().clone();
            let line = store.line.lock().unwrap().clone();
            if let (Some(room), Some(quality)) = (room, quality) {
                match crate::platforms::douyu::get_stream_url_with_quality(&room, &quality, line.as_deref()).await {
                    Ok(fresh) => {
                        println!("[proxy] refresher: renewed Douyu URL for room {}", room);
                        *store.url.lock().unwrap() = fresh;
                    }
                    Err(e) => eprintln!("[proxy] refresher: renew failed: {}", e),
                }
            }
        }
        });
    });
}

// ---------------------------------------------------------------------------
// Single shared upstream pump. Douyu/Huya CDNs allow only ONE connection per
// signed URL; per-client upstream connections would fight each other (the TV
// and the local player kept killing one another). One pump drains the CDN at
// full speed and fans out to every client; late joiners receive the cached FLV
// header first so their decoders always start with a valid file header.
// ---------------------------------------------------------------------------

struct SharedPump {
    tx: broadcast::Sender<(u64, bytes::Bytes)>,
    header: StdMutex<Option<(u64, bytes::Bytes)>>,
    header_sent: AtomicBool,
    cancelled: AtomicBool,
}

static PUMP: OnceLock<StdMutex<Option<(usize, Arc<SharedPump>)>>> = OnceLock::new();

fn pump_slot() -> &'static StdMutex<Option<(usize, Arc<SharedPump>)>> {
    PUMP.get_or_init(|| StdMutex::new(None))
}

/// Returns the pump bound to this store; a different store (room/platform
/// switch) replaces the previous pump and cancels it.
fn pump_for(store: &StreamUrlStore) -> Arc<SharedPump> {
    // Key on the shared url Arc: stable across Data clones and mutations,
    // unique per stream source (and per test).
    let key = Arc::as_ptr(&store.url) as usize;
    let slot = pump_slot().lock().unwrap();
    if let Some((k, p)) = slot.as_ref() {
        if *k == key && !p.cancelled.load(Ordering::SeqCst) {
            return p.clone();
        }
    }
    drop(slot);
    let (tx, _rx) = broadcast::channel::<(u64, bytes::Bytes)>(1024);
    let p = Arc::new(SharedPump {
        tx,
        header: StdMutex::new(None),
        header_sent: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
    });
    let mut slot = pump_slot().lock().unwrap();
    if let Some((_, old)) = slot.take() {
        old.cancelled.store(true, Ordering::SeqCst);
    }
    *slot = Some((key, p.clone()));
    p
}

async fn refresh_store_url(
    store: &StreamUrlStore,
    expect_gen: u64,
    cancelled: &AtomicBool,
) -> bool {
    // Stale refresh guard: if the source changed (room/platform switch) or the
    // pump was cancelled while we were re-signing, DO NOT write the freshly
    // signed URL — it belongs to the previous stream and would hijack the new
    // pump (user-visible as "switched room plays the old room / fails").
    let stale = || {
        cancelled.load(Ordering::SeqCst)
            || store.generation.load(Ordering::SeqCst) != expect_gen
    };
    if stale() {
        return false;
    }
    let platform = store.platform.lock().unwrap().clone();
    match platform.as_deref() {
        Some("douyu") => {
            let room = store.room_id.lock().unwrap().clone();
            let quality = store.quality.lock().unwrap().clone();
            let line = store.line.lock().unwrap().clone();
            if let (Some(room), Some(quality)) = (room, quality) {
                match crate::platforms::douyu::get_stream_url_with_quality(
                    &room,
                    &quality,
                    line.as_deref(),
                )
                .await
                {
                    Ok(fresh) => {
                        if stale() {
                            println!("[proxy] dropped stale Douyu refresh for room {}", room);
                            return false;
                        }
                        println!("[proxy] refreshed Douyu URL for room {}", room);
                        *store.url.lock().unwrap() = fresh;
                        true
                    }
                    Err(e) => {
                        eprintln!("[proxy] Douyu refresh failed: {}", e);
                        false
                    }
                }
            } else {
                false
            }
        }
        Some("huya") => {
            let room = store.room_id.lock().unwrap().clone();
            let quality = store.quality.lock().unwrap().clone();
            let line = store.line.lock().unwrap().clone();
            let client = store.huya_follow_http.lock().unwrap().clone();
            if let (Some(room), Some(client)) = (room, client) {
                match crate::platforms::huya::stream_url::fetch_huya_flv_url(
                    &client, &room, quality.as_deref(), line.as_deref(),
                )
                .await
                {
                    Ok(fresh) => {
                        if stale() {
                            println!("[proxy] dropped stale Huya refresh for room {}", room);
                            return false;
                        }
                        println!("[proxy] refreshed Huya URL for room {}", room);
                        *store.url.lock().unwrap() = fresh;
                        true
                    }
                    Err(e) => {
                        eprintln!("[proxy] Huya refresh failed: {}", e);
                        false
                    }
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Called when the stream source changes (room/quality/platform switch) so no
/// client can receive a stale cached FLV header for the previous stream.
pub fn reset_pump_header() {
    if let Some(slot) = PUMP.get() {
        if let Some((_, p)) = slot.lock().unwrap().as_ref() {
            *p.header.lock().unwrap() = None;
            p.header_sent.store(false, Ordering::SeqCst);
        }
    }
}

/// Called when the stream source changes (room/quality/platform switch) so no
/// client can receive a stale cached FLV header for the previous stream.

fn ensure_pump(store: StreamUrlStore) {
    let p = pump_for(&store);
    if p.cancelled.load(Ordering::SeqCst) {
        return;
    }

    let client = Client::builder()
        .no_proxy()
        .http1_only()
        .gzip(false)
        .brotli(false)
        .no_deflate()
        // No connection reuse: Douyu binds a signature to the TCP session, so
        // a pooled old connection makes a freshly signed URL fail instantly.
        .pool_max_idle_per_host(0)
        .tcp_keepalive(Duration::from_secs(60))
        .timeout(Duration::from_secs(7200))
        .build()
        .expect("failed to build pump client");

    tokio::spawn(async move {
        let tx = p.tx.clone();
        let mut attempts: u32 = 0;
        while attempts < 1000 && !p.cancelled.load(Ordering::SeqCst) {
            let url = store.url.lock().unwrap().clone();
            if url.is_empty() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if url.contains(":34719/live.flv") || url.contains(":34721/live.flv") {
                eprintln!("[proxy] pump refused self-referential URL");
                break;
            }

            let gen_at_connect = store.generation.load(Ordering::SeqCst);
            let connected_at = std::time::Instant::now();
            let mut rejected = false;
            match build_upstream_request(&client, &url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    attempts = 0;
                    let mut stream = resp.bytes_stream();
                    let mut is_first = true;
                    while let Some(chunk) = stream.next().await {
                        if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                            println!("[proxy] stream source changed -> switch upstream");
                            break;
                        }
                        match chunk {
                            Ok(bytes) => {
                                if is_first {
                                    is_first = false;
                                    let cached_gen = p
                                        .header
                                        .lock()
                                        .unwrap()
                                        .as_ref()
                                        .map(|(g, _)| *g);
                                    if cached_gen == Some(gen_at_connect) {
                                        // True reconnect of the SAME source: clients already
                                        // hold this header, so drop the duplicate.
                                        println!("[proxy] dropped duplicate FLV header on reconnect");
                                        continue;
                                    }
                                    // New source (room/platform switch): publish its header
                                    *p.header.lock().unwrap() =
                                        Some((gen_at_connect, bytes.clone()));
                                }
                                let _ = tx.send((gen_at_connect, bytes));
                            }
                            Err(e) => {
                                eprintln!("[proxy] pump chunk error: {} -> reconnect", e);
                                break;
                            }
                        }
                    }
                    println!("[proxy] pump upstream EOF -> refresh URL and reconnect");
                }
                Ok(resp) => {
                    eprintln!("[proxy] pump upstream status {} -> retry", resp.status());
                    rejected = true;
                }
                Err(e) => eprintln!("[proxy] pump connect error: {} -> retry", e),
            }

            // If the source changed (room/quality switch), reset header state so
            // the new stream's FLV header is broadcast once to all clients.
            if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                *p.header.lock().unwrap() = None;
                p.header_sent.store(false, Ordering::SeqCst);
                continue;
            }
            attempts += 1;
            // Only re-sign when the URL is actually unusable: Douyu invalidates
            // the previous signature as soon as a new one is issued, so
            // refreshing on every reconnect would kill our own new connection.
            let lived = connected_at.elapsed();
            // The connection ended (EOF) or was rejected: the signature is
            // consumed/invalid, always fetch a fresh one before reconnecting.
            println!(
                "[proxy] re-signing upstream (rejected={} lived={:?})",
                rejected, lived
            );
            refresh_store_url(&store, gen_at_connect, &p.cancelled).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });
}

struct ClientStream {
    inner: tokio_stream::wrappers::BroadcastStream<(u64, bytes::Bytes)>,
    header: Option<bytes::Bytes>,
    expect_gen: Option<u64>,
}

impl futures_util::Stream for ClientStream {
    type Item = Result<bytes::Bytes, actix_web::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(h) = self.header.take() {
            return std::task::Poll::Ready(Some(Ok(h)));
        }
        loop {
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok((gen, bytes)))) => {
                    match self.expect_gen {
                        None => self.expect_gen = Some(gen),
                        Some(g) if g != gen => {
                            // Source switched mid-stream: end this response so the
                            // client reconnects cleanly to the new stream.
                            return std::task::Poll::Ready(None);
                        }
                        Some(_) => {}
                    }
                    return std::task::Poll::Ready(Some(Ok(bytes)));
                }
                std::task::Poll::Ready(Some(Err(_))) => continue, // lagged: skip to live edge
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

async fn flv_proxy_handler(
    _req: HttpRequest,
    stream_url_store: web::Data<StreamUrlStore>,
) -> impl Responder {
    let url = stream_url_store.url.lock().unwrap().clone();
    if url.is_empty() {
        return HttpResponse::NotFound().body("Stream URL is not set or empty.");
    }
    println!("[Rust/proxy.rs handler] Incoming FLV proxy request -> {}", url);

    let store: StreamUrlStore = (**stream_url_store).clone();
    ensure_pump(store.clone());

    let p = pump_for(&store);
    let rx = p.tx.subscribe();
    let current_gen = store.generation.load(Ordering::SeqCst);
    let mut header = p
        .header
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(g, _)| *g == current_gen)
        .map(|(_, b)| b.clone());
    HttpResponse::Ok()
        .content_type("video/x-flv")
        .insert_header(("Connection", "keep-alive"))
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("Accept-Ranges", "bytes"))
        .streaming(ClientStream {
            inner: tokio_stream::wrappers::BroadcastStream::new(rx),
            header,
            expect_gen: None,
        })
}

#[derive(Deserialize)]
struct ImageQuery { url: String }

async fn image_proxy_handler(
    query: web::Query<ImageQuery>,
    client: web::Data<Client>,
) -> impl Responder {
    let url = query.url.clone();
    if url.is_empty() { return HttpResponse::BadRequest().body("Missing url query parameter"); }

    let mut req = client.get(&url)
        .header("User-Agent","Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .header("Accept","image/avif,image/webp,image/apng,image/*;q=0.8,*/*;q=0.5");

    if url.contains("hdslb.com") || url.contains("bilibili.com") {
        req = req.header("Referer","https://live.bilibili.com/").header("Origin","https://live.bilibili.com");
    } else if url.contains("huya.com") {
        req = req.header("Referer","https://www.huya.com/").header("Origin","https://www.huya.com");
    } else if url.contains("douyin") || url.contains("douyinpic.com") {
        req = req.header("Referer","https://www.douyin.com/");
    }

    match req.send().await {
        Ok(upstream_response) => {
            let content_type = upstream_response.headers().get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
            if upstream_response.status().is_success() {
                match upstream_response.bytes().await {
                    Ok(bytes) => HttpResponse::Ok().content_type(content_type)
                        .insert_header(("Content-Length", bytes.len().to_string()))
                        .insert_header(("Cache-Control", "public, max-age=86400, immutable"))
                        .body(bytes),
                    Err(e) => HttpResponse::InternalServerError().body(format!("Failed: {}", e)),
                }
            } else {
                HttpResponse::build(
                    actix_web::http::StatusCode::from_u16(upstream_response.status().as_u16())
                        .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR),
                )
                .body(format!("Upstream error: {}", upstream_response.status()))
            }
        }
        Err(e) => HttpResponse::InternalServerError().body(format!("Connection error: {}", e)),
    }
}

pub async fn start_proxy_inner(
    server_handle: Arc<StdMutex<Option<ServerHandle>>>,
    stream_url_store: StreamUrlStore,
) -> Result<String, String> {
    let port = find_free_port().await;
    let current_stream_url = stream_url_store.url.lock().unwrap().clone();
    if current_stream_url.is_empty() {
        return Err("Stream URL is not set in store. Cannot start proxy.".to_string());
    }

    spawn_url_refresher(stream_url_store.clone());

    let stream_url_data_for_actix = web::Data::new(stream_url_store.clone());

    let existing_handle_to_stop = { server_handle.lock().unwrap().take() };
    if let Some(existing_handle) = existing_handle_to_stop {
        existing_handle.stop(false).await;
    }

    let server = match HttpServer::new(move || {
        let app_data_stream_url = stream_url_data_for_actix.clone();
        let app_data_reqwest_client = web::Data::new(
            Client::builder()
                .no_proxy()
                .http1_only()
                .gzip(false)
                .brotli(false)
                .no_deflate()
                .pool_idle_timeout(None)
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(60))
                .timeout(Duration::from_secs(7200))
                .build()
                .expect("failed to build client"),
        );
        App::new()
            .app_data(app_data_stream_url)
            .app_data(app_data_reqwest_client)
            .wrap(actix_cors::Cors::permissive())
            .route("/live.flv", web::get().to(flv_proxy_handler))
            .route("/image", web::get().to(image_proxy_handler))
    })
    .keep_alive(Duration::from_secs(86400))
    .bind(("0.0.0.0", port))
    {
        Ok(srv) => srv,
        Err(e) => {
            return Err(format!("[Rust/proxy.rs] Failed to bind server to port {}: {}", port, e));
        }
    }
    .run();

    let server_handle_for_state = server.handle();
    *server_handle.lock().unwrap() = Some(server_handle_for_state);

    tauri::async_runtime::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("[Rust/proxy.rs] Proxy server run error: {}", e);
        }
    });

    let gen = stream_url_store.generation.load(Ordering::SeqCst);
    Ok(format!("http://127.0.0.1:{}/live.flv?v={}", port, gen))
}

#[tauri::command]
pub async fn start_proxy(
    _app_handle: AppHandle,
    server_handle_state: State<'_, ProxyServerHandle>,
    stream_url_store: State<'_, StreamUrlStore>,
) -> Result<String, String> {
    start_proxy_inner(server_handle_state.0.clone(), stream_url_store.inner().clone()).await
}

#[tauri::command]
pub async fn stop_proxy(
    server_handle_state: State<'_, ProxyServerHandle>,
) -> Result<(), String> {
    let handle = { server_handle_state.0.lock().unwrap().take() };
    if let Some(handle) = handle {
        handle.stop(false).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn start_static_proxy_server(
    _app_handle: AppHandle,
    stream_url_store: State<'_, StreamUrlStore>,
) -> Result<String, String> {
    let port: u16 = 34721;
    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
        return Ok(format!("http://127.0.0.1:{}", port));
    }

    let stream_url_data_for_actix = web::Data::new(stream_url_store.inner().clone());

    let server = match HttpServer::new(move || {
        let app_data_stream_url = stream_url_data_for_actix.clone();
        let app_data_reqwest_client = web::Data::new(
            Client::builder()
                .no_proxy()
                .http1_only()
                .gzip(false)
                .brotli(false)
                .no_deflate()
                .pool_idle_timeout(None)
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(60))
                .timeout(Duration::from_secs(7200))
                .build()
                .expect("failed to build client"),
        );
        App::new()
            .app_data(app_data_stream_url)
            .app_data(app_data_reqwest_client)
            .wrap(actix_cors::Cors::permissive())
            .route("/live.flv", web::get().to(flv_proxy_handler))
            .route("/image", web::get().to(image_proxy_handler))
    })
    .keep_alive(Duration::from_secs(86400))
    .bind(("127.0.0.1", port))
    {
        Ok(srv) => srv,
        Err(e) => {
            if e.kind() == ErrorKind::AddrInUse {
                return Ok(format!("http://127.0.0.1:{}", port));
            }
            return Err(format!("Failed to bind static proxy: {}", e));
        }
    }
    .run();

    tauri::async_runtime::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("[Rust/proxy.rs] static proxy error: {}", e);
        }
    });

    Ok(format!("http://127.0.0.1:{}", port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock CDN: streams FLV header + 3x20KB per connection, then drops.
    async fn mock_cdn() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf).await;
                        let head = b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nTransfer-Encoding: chunked\r\n\r\n";
                        let _ = sock.write_all(head).await;
                        let flv_head: &[u8] = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00";
                        let framed = format!("{:x}\r\n", flv_head.len());
                        let _ = sock.write_all(framed.as_bytes()).await;
                        let _ = sock.write_all(flv_head).await;
                        let _ = sock.write_all(b"\r\n").await;
                        for _ in 0..3 {
                            let chunk = vec![0xABu8; 20480];
                            let framed = format!("{:x}\r\n", chunk.len());
                            let _ = sock.write_all(framed.as_bytes()).await;
                            let _ = sock.write_all(&chunk).await;
                            let _ = sock.write_all(b"\r\n").await;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        let _ = sock.shutdown().await;
                    });
                }
            }
        });
        port
    }

    pub(super) async fn spawn_test_server(store: StreamUrlStore, port: u16) {
        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(store.clone()))
                .app_data(web::Data::new(
                    Client::builder().no_proxy().http1_only().build().unwrap(),
                ))
                .route("/live.flv", web::get().to(flv_proxy_handler))
        })
        .bind(("127.0.0.1", port))
        .unwrap()
        .workers(1)
        .run();
        actix_web::rt::spawn(server);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    #[actix_web::test]
    async fn late_joining_client_gets_flv_header() {
        let cdn_port = mock_cdn().await;
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_port);
        spawn_test_server(store, 39916).await;

        let client = Client::builder().no_proxy().build().unwrap();

        // client 1 (local player) joins first
        let resp1 = client.get("http://127.0.0.1:39916/live.flv").send().await.unwrap();
        let mut s1 = resp1.bytes_stream();
        let mut total1 = 0usize;
        for _ in 0..5 {
            if let Ok(Some(Ok(b))) = tokio::time::timeout(Duration::from_millis(800), s1.next()).await {
                total1 += b.len();
            }
        }
        assert!(total1 > 0);

        // client 2 (the TV) joins mid-stream: must still start with FLV header
        let resp2 = client.get("http://127.0.0.1:39916/live.flv").send().await.unwrap();
        let mut s2 = resp2.bytes_stream();
        let mut first3 = Vec::new();
        let mut total2 = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while first3.len() < 3 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s2.next()).await {
                Ok(Some(Ok(b))) => {
                    if first3.len() < 3 { first3.extend_from_slice(&b[..3.min(b.len())]); }
                    total2 += b.len();
                }
                Ok(Some(Err(_))) => break,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert_eq!(&first3[..3], b"FLV", "late joiner must receive FLV header");
        assert!(total2 > 0);
        println!("late joiner got FLV header + {} bytes", total2);
    }

    #[actix_web::test]
    async fn room_switch_never_mixes_streams() {
        let cdn_a = mock_cdn_tagged(b'A').await;
        let cdn_b = mock_cdn_tagged(b'B').await;

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_a);
        spawn_test_server(store.clone(), 39917).await;

        let client = Client::builder().no_proxy().build().unwrap();

        // client on room A
        let resp_a = client.get("http://127.0.0.1:39917/live.flv").send().await.unwrap();
        let mut sa = resp_a.bytes_stream();
        let mut got_a = Vec::new();
        for _ in 0..4 {
            if let Ok(Some(Ok(b))) = tokio::time::timeout(Duration::from_millis(700), sa.next()).await {
                got_a.extend_from_slice(&b);
            }
        }
        assert!(got_a.starts_with(b"FLV"), "room A must start with FLV");

        // switch to room B (as the frontend does via set_stream_url_cmd)
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_b);
        store.generation.fetch_add(1, Ordering::SeqCst);
        crate::proxy::reset_pump_header();
        tokio::time::sleep(Duration::from_millis(2500)).await; // let pump switch

        // new client for room B
        let resp_b = client.get("http://127.0.0.1:39917/live.flv").send().await.unwrap();
        let mut sb = resp_b.bytes_stream();
        let mut got_b = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while got_b.len() < 4096 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(700), sb.next()).await {
                Ok(Some(Ok(b))) => got_b.extend_from_slice(&b),
                Ok(Some(Err(_))) => break,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(got_b.starts_with(b"FLV"), "room B must start with FLV header");
        // body payload of B is 0xBB; ensure no A payload (0xAA) leaked in
        let body_b = &got_b[11.min(got_b.len())..];
        assert!(!body_b.contains(&0x41), "room A payload leaked into room B stream");
        assert!(body_b.contains(&0x42), "room B payload missing");
        println!("room switch clean: B starts with FLV, no A payload leaked");
    }

    /// Mock CDN with tagged payload bytes.
    async fn mock_cdn_tagged(tag: u8) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut sock, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 2048];
                        let _ = sock.read(&mut buf).await;
                        let head = b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nTransfer-Encoding: chunked\r\n\r\n";
                        let _ = sock.write_all(head).await;
                        let flv_head: &[u8] = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00";
                        let framed = format!("{:x}\r\n", flv_head.len());
                        let _ = sock.write_all(framed.as_bytes()).await;
                        let _ = sock.write_all(flv_head).await;
                        let _ = sock.write_all(b"\r\n").await;
                        loop {
                            let chunk = vec![tag; 20480];
                            let framed = format!("{:x}\r\n", chunk.len());
                            let _ = sock.write_all(framed.as_bytes()).await;
                            let _ = sock.write_all(&chunk).await;
                            let _ = sock.write_all(b"\r\n").await;
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    });
                }
            }
        });
        port
    }

    #[actix_web::test]
    async fn client_sees_flv_header_and_survives_cdn_drops() {
        let cdn_port = mock_cdn().await;
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_port);
        spawn_test_server(store, 39914).await;

        let client = Client::builder().no_proxy().build().unwrap();
        let resp = client.get("http://127.0.0.1:39914/live.flv").send().await.unwrap();
        assert!(resp.status().is_success());

        let mut stream = resp.bytes_stream();
        let mut total = 0usize;
        let mut first3 = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(600), stream.next()).await {
                Ok(Some(Ok(b))) => {
                    if first3.len() < 3 {
                        first3.extend_from_slice(&b[..3.min(b.len())]);
                    }
                    total += b.len();
                }
                Ok(Some(Err(_))) => break,
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert_eq!(&first3[..3], b"FLV", "client must receive FLV header first");
        // each connection: 11 + 61440 bytes; 8s window spans >= 2 connections
        assert!(total >= 61451 + 61440, "reconnect failed, got {} bytes", total);
        println!("client received {} bytes, starts with FLV header", total);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Real Douyu CDN through the real HTTP endpoint (the exact cast path).
    #[actix_web::test]
    #[ignore]
    async fn live_douyu_http_endpoint_streams() {
        let room = std::env::var("DTV_TEST_ROOM").unwrap_or_else(|_| "9999".to_string());
        let url = crate::platforms::douyu::get_stream_url(&room, None)
            .await
            .expect("failed to get douyu stream url");

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = url;
        *store.platform.lock().unwrap() = Some("douyu".into());
        *store.room_id.lock().unwrap() = Some(room);
        *store.quality.lock().unwrap() = Some("原画".into());

        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(store.clone()))
                .app_data(web::Data::new(
                    Client::builder().no_proxy().http1_only().build().unwrap(),
                ))
                .route("/live.flv", web::get().to(flv_proxy_handler))
        })
        .bind(("127.0.0.1", 39913))
        .unwrap()
        .workers(1)
        .run();
        actix_web::rt::spawn(server);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let client = Client::builder().no_proxy().build().unwrap();
        let resp = client.get("http://127.0.0.1:39913/live.flv").send().await.unwrap();
        assert!(resp.status().is_success());
        let mut stream = resp.bytes_stream();
        let mut total = 0usize;
        let mut first3 = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), stream.next()).await {
                Ok(Some(Ok(b))) => {
                    if first3.len() < 3 { first3.extend_from_slice(&b[..3.min(b.len())]); }
                    total += b.len();
                }
                Ok(Some(Err(e))) => panic!("stream error: {}", e),
                Ok(None) => panic!("stream ended early after {} bytes", total),
                Err(_) => continue,
            }
        }
        assert_eq!(&first3[..3], b"FLV", "must start with FLV header");
        println!("HTTP endpoint delivered {} bytes in 20s, starts with FLV", total);
        assert!(total > 500_000, "too little data via HTTP: {}", total);
    }
}

#[cfg(test)]
mod live_tests_all_platforms {
    use super::*;

    async fn verify_stream(name: &str, url: String, expect_flv: bool) {
        println!("[live:{}] upstream: {}", name, url);
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = url.clone();
        *store.platform.lock().unwrap() = Some(name.to_string());

        let port: u16 = match name {
            "douyin" => 39921,
            "huya" => 39922,
            "bilibili" => 39923,
            _ => 39924,
        };
        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(store.clone()))
                .app_data(web::Data::new(
                    Client::builder().no_proxy().http1_only().build().unwrap(),
                ))
                .route("/live.flv", web::get().to(flv_proxy_handler))
        })
        .bind(("127.0.0.1", port))
        .unwrap()
        .workers(1)
        .run();
        actix_web::rt::spawn(server);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let client = Client::builder().no_proxy().build().unwrap();
        let resp = client
            .get(format!("http://127.0.0.1:{}/live.flv", port))
            .send()
            .await
            .expect("proxy request failed");
        assert!(resp.status().is_success(), "{}: proxy status {}", name, resp.status());
        let mut stream = resp.bytes_stream();
        let mut total = 0usize;
        let mut first3 = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), stream.next()).await {
                Ok(Some(Ok(b))) => {
                    if first3.len() < 3 { first3.extend_from_slice(&b[..3.min(b.len())]); }
                    total += b.len();
                }
                Ok(Some(Err(e))) => panic!("[live:{}] stream error: {}", name, e),
                Ok(None) => panic!("[live:{}] stream ended early at {} bytes", name, total),
                Err(_) => continue,
            }
        }
        if expect_flv {
            assert_eq!(&first3[..3], b"FLV", "[live:{}] missing FLV header", name);
        }
        println!("[live:{}] delivered {} bytes in 15s (flv={})", name, total, expect_flv);
        assert!(total > 200_000, "[live:{}] too little data: {}", name, total);
    }

    #[actix_web::test]
    #[ignore]
    async fn live_huya_streams() {
        let room = std::env::var("DTV_TEST_HUYA_ROOM").unwrap_or_else(|_| "660002".to_string());
        let client = crate::platforms::common::FollowHttpClient::new().unwrap();
        let url = crate::platforms::huya::stream_url::fetch_huya_flv_url(&client, &room, None, None)
            .await
            .expect("huya url");
        verify_stream("huya", url, true).await;
    }

    #[actix_web::test]
    #[ignore]
    async fn live_douyin_streams() {
        let candidates: Vec<String> = std::env::var("DTV_TEST_DOUYIN_ROOM")
            .map(|v| vec![v])
            .unwrap_or_else(|_| {
                vec![
                    "278262933884".into(),
                    "795962752690".into(),
                    "666052985560".into(),
                    "474743113983".into(),
                ]
            });
        // Prefer discovering a live room from the partition list
        for (partition, ptype) in [("1", "1"), ("game", "1"), ("720", "1")] {
            if let Ok(list) = crate::platforms::douyin::douyin_streamer_list::fetch_douyin_partition_rooms_core(
                partition.to_string(),
                ptype.to_string(),
                0,
                String::new(),
            )
            .await
            {
                for room in list.rooms.iter().take(6) {
                    let rid = room.web_rid.clone();
                    if let Ok(i) = crate::platforms::douyin::douyin_streamer_detail::fetch_douyin_stream_core(
                        rid.clone(),
                        "原画".to_string(),
                    )
                    .await
                    {
                        if i.status == Some(2) && i.stream_url.is_some() {
                            println!("[live:douyin] discovered live room {} via partition {}", rid, partition);
                            let url = i.stream_url.expect("url");
                            let is_flv = url.contains(".flv");
                            verify_stream("douyin", url, is_flv).await;
                            return;
                        }
                    }
                }
            }
        }
        let mut info = None;
        for room in &candidates {
            match crate::platforms::douyin::douyin_streamer_detail::fetch_douyin_stream_core(
                room.clone(),
                "原画".to_string(),
            )
            .await
            {
                Ok(i) if i.status == Some(2) && i.stream_url.is_some() => {
                    println!("[live:douyin] using live room {}", room);
                    info = Some(i);
                    break;
                }
                Ok(_) => println!("[live:douyin] room {} not live", room),
                Err(e) => println!("[live:douyin] room {} error: {}", room, e),
            }
        }
        let info = info.expect("no live douyin room found among candidates");
        let url = info.stream_url.expect("douyin url");
        let is_flv = url.contains(".flv");
        verify_stream("douyin", url, is_flv).await;
    }

    #[actix_web::test]
    #[ignore]
    async fn live_bilibili_streams() {
        let room = std::env::var("DTV_TEST_BILI_ROOM").unwrap_or_else(|_| "252140".to_string());
        let info = crate::platforms::bilibili::stream_url::fetch_bilibili_stream_core(
            room,
            "原画".to_string(),
            None,
            None,
        )
        .await
        .expect("bilibili info");
        let url = info
            .upstream_url
            .clone()
            .or(info.stream_url.clone())
            .expect("bilibili url");
        let is_flv = !url.contains(".m3u8");
        verify_stream("bilibili", url, is_flv).await;
    }
}

#[cfg(test)]
mod live_cast_tv {
    use super::*;

    /// End-to-end cast test: real Douyu CDN -> proxy -> real TV (DLNA SOAP).
    /// Requires DTV_TV_HOST (e.g. 192.168.31.211) and DTV_TV_CTRL (control URL).
    #[actix_web::test]
    #[ignore]
    async fn live_cast_to_tv() {
        let tv_host = std::env::var("DTV_TV_HOST").expect("DTV_TV_HOST");
        let tv_ctrl = std::env::var("DTV_TV_CTRL").expect("DTV_TV_CTRL");
        let room = std::env::var("DTV_TEST_ROOM").unwrap_or_else(|_| "9999".to_string());

        let url = crate::platforms::douyu::get_stream_url(&room, None)
            .await
            .expect("douyu url");

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = url;
        *store.platform.lock().unwrap() = Some("douyu".into());
        *store.room_id.lock().unwrap() = Some(room);
        *store.quality.lock().unwrap() = Some("原画".into());

        let port: u16 = 39930;
        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(store.clone()))
                .app_data(web::Data::new(
                    Client::builder().no_proxy().http1_only().build().unwrap(),
                ))
                .route("/live.flv", web::get().to(flv_proxy_handler))
        })
        .bind(("0.0.0.0", port))
        .unwrap()
        .workers(1)
        .run();
        actix_web::rt::spawn(server);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let lan = local_ip_address::local_ip().expect("lan ip");
        let stream = format!("http://{}:{}/live.flv", lan, port);
        println!("[cast] pushing {} to TV {}", stream, tv_host);

        let meta = format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="dtv-live" parentID="0" restricted="1"><dc:title>DTV Live</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:video/x-flv:*">{}</res></item></DIDL-Lite>"#,
            stream
        );
        let client = Client::builder().no_proxy().timeout(Duration::from_secs(10)).build().unwrap();
        let set_soap = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#,
            stream,
            html_escape::encode_safe(&meta)
        );
        let r = client.post(&tv_ctrl)
            .header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"")
            .header("Content-Type", "text/xml; charset=utf-8")
            .body(set_soap).send().await.expect("set uri");
        assert!(r.status().is_success());
        tokio::time::sleep(Duration::from_millis(500)).await;
        let play_soap = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"#;
        let r = client.post(&tv_ctrl)
            .header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#Play\"")
            .header("Content-Type", "text/xml; charset=utf-8")
            .body(play_soap).send().await.expect("play");
        assert!(r.status().is_success());

        // Observation window (default 12s; set DTV_CAST_OBSERVE_SECS for long runs)
        let observe: u64 = std::env::var("DTV_CAST_OBSERVE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(12);
        let mut t = 0u64;
        while t < observe {
            tokio::time::sleep(Duration::from_secs(15)).await;
            t += 15;
            let resp = client.post(&tv_ctrl)
                .header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\"")
                .header("Content-Type", "text/xml; charset=utf-8")
                .body(info_soap_at(t)).send().await;
            let body = match resp {
                Ok(r) => r.text().await.unwrap_or_default(),
                Err(_) => String::new(),
            };
            let state = body.split("<CurrentTransportState>").nth(1).and_then(|s| s.split('<').next()).unwrap_or("?");
            println!("[cast] t={}s TV state={}", t, state);
            assert_eq!(state, "PLAYING", "TV stopped playing at t={}s", t);
        }
        fn info_soap_at(_t: u64) -> String {
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID></u:GetTransportInfo></s:Body></s:Envelope>"#.to_string()
        }
    }
}

#[cfg(test)]
mod live_switch_tests {
    use super::*;
    use super::tests::spawn_test_server;

    /// Reproduces the frontend's exact room-switch sequence against TWO LIVE
    /// Douyu rooms: play A via proxy -> stop_proxy -> set B (+gen/reset) ->
    /// start_proxy -> client connects immediately with the returned URL.
    #[actix_web::test]
    #[ignore]
    async fn live_room_switch_sequence() {
        let rooms = vec!["9999".to_string(), "12821".to_string()];
        let urls: Vec<String> = {
            let mut v = vec![];
            for r in &rooms {
                v.push(
                    crate::platforms::douyu::get_stream_url(r, None)
                        .await
                        .expect("douyu url"),
                );
            }
            v
        };
        println!("[switch] room A url len={} room B url len={}", urls[0].len(), urls[1].len());

        let store = StreamUrlStore::default();

        // --- play room A ---
        *store.url.lock().unwrap() = urls[0].clone();
        *store.platform.lock().unwrap() = Some("douyu".into());
        store.generation.fetch_add(1, Ordering::SeqCst);
        reset_pump_header();
        spawn_test_server(store.clone(), 39918).await;
        let url_a = "http://127.0.0.1:39918/live.flv".to_string();
        println!("[switch] proxy url A: {}", url_a);

        let client = Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url_a).send().await.expect("get A");
        let mut s = resp.bytes_stream();
        let mut head_a = Vec::new();
        let mut total_a = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        while total_a < 200_000 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s.next()).await {
                Ok(Some(Ok(b))) => {
                    if head_a.len() < 3 { head_a.extend_from_slice(&b[..3.min(b.len())]); }
                    total_a += b.len();
                }
                Ok(Some(Err(e))) => panic!("A stream error: {}", e),
                Ok(None) => panic!("A ended early"),
                Err(_) => continue,
            }
        }
        assert_eq!(&head_a[..3], b"FLV");
        println!("[switch] room A played {} bytes", total_a);

        // --- switch exactly like reloadStream: set new url + bump generation ---
        *store.url.lock().unwrap() = urls[1].clone();
        store.generation.fetch_add(1, Ordering::SeqCst);
        reset_pump_header();
        let url_b = "http://127.0.0.1:39918/live.flv?v=2".to_string();
        println!("[switch] proxy url B: {}", url_b);

        // client connects IMMEDIATELY (worst case: before pump has B's header)
        let resp = client.get(&url_b).send().await.expect("get B");
        let mut s = resp.bytes_stream();
        let mut head_b = Vec::new();
        let mut total_b = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while total_b < 200_000 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s.next()).await {
                Ok(Some(Ok(b))) => {
                    if head_b.len() < 3 { head_b.extend_from_slice(&b[..3.min(b.len())]); }
                    total_b += b.len();
                }
                Ok(Some(Err(e))) => panic!("B stream error: {}", e),
                Ok(None) => panic!("B ended early"),
                Err(_) => continue,
            }
        }
        assert_eq!(&head_b[..3], b"FLV", "room B must start with FLV header");
        println!("[switch] room B played {} bytes after switch", total_b);

    }
}
