use actix_web::{dev::ServerHandle, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use futures_util::StreamExt;
use reqwest::Client;
use crate::StreamUrlStore;
use serde::Deserialize;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::broadcast;
use std::time::Duration;
use tauri::{AppHandle, State};

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
    tx: broadcast::Sender<(u64, u64, bytes::Bytes)>,
    /// What a late-joining client needs to start decoding (see `Primer`).
    primer: StdMutex<Primer>,
    header_sent: AtomicBool,
    cancelled: AtomicBool,
    /// True while this pump's single upstream task is running. It is re-spawned
    /// by the next request when false: `start_proxy` recreates the actix server
    /// on every room switch and that teardown kills tasks spawned on it.
    task_running: AtomicBool,
    /// Consecutive respawns without a healthy session; bounded so a permanently
    /// broken source cannot spin forever. Any session that actually streams
    /// resets it, so ordinary use (room switches) is unlimited.
    task_spawns: AtomicUsize,
    /// Set once the current upstream session has moved real data.
    session_healthy: AtomicBool,
    /// Set once this pump has broadcast any byte of the current generation, so
    /// a client that connects before that simply reads the stream from its true
    /// start instead of needing a primer.
    broadcast_started: AtomicBool,
    /// Monotonic id for diagnostics: tells concurrent pumps apart in the log.
    id: u64,
}

const MAX_FLV_TAG: usize = 4 * 1024 * 1024;
const MAX_PENDING: usize = 8 * 1024 * 1024;
const MAX_KEYFRAME_BLOCK: usize = 6 * 1024 * 1024;

/// One parsed unit of an FLV byte stream.
enum FlvUnit {
    /// The 9-byte FLV header plus its PreviousTagSize0 (first 13 bytes).
    Header(bytes::Bytes),
    /// A complete FLV tag (11-byte header + body + trailing PreviousTagSize).
    Tag {
        bytes: bytes::Bytes,
        /// True for a video keyframe tag (a safe mid-stream resume point).
        is_keyframe: bool,
    },
    /// Not parseable as FLV (mock/test sources): pass through untouched so the
    /// framer never blocks a non-FLV stream.
    Raw(bytes::Bytes),
}

/// Splits an FLV byte stream into whole tags so the pump never puts a partial
/// tag on the wire. A partial tag at a CDN cut, or a mid-tag resume after a
/// re-sign reconnect, desyncs the decoder and shows mosaic until the next
/// keyframe — exactly the "看着看着就有马赛克，过一会儿又好了" symptom.
struct FlvFramer {
    pending: Vec<u8>,
    got_header: bool,
    /// Set once the stream proved not to be FLV; from then on bytes pass raw.
    unparsable: bool,
}

impl FlvFramer {
    fn new() -> Self {
        Self { pending: Vec::new(), got_header: false, unparsable: false }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<FlvUnit> {
        let mut out = Vec::new();
        if self.unparsable {
            out.push(FlvUnit::Raw(bytes::Bytes::copy_from_slice(chunk)));
            return out;
        }
        self.pending.extend_from_slice(chunk);

        if !self.got_header {
            if self.pending.len() < 13 {
                return out;
            }
            if &self.pending[..3] != b"FLV" {
                // Not FLV: emit everything buffered as raw and stay in raw mode.
                self.unparsable = true;
                let all = std::mem::take(&mut self.pending);
                out.push(FlvUnit::Raw(bytes::Bytes::from(all)));
                return out;
            }
            let header: Vec<u8> = self.pending.drain(..13).collect();
            out.push(FlvUnit::Header(bytes::Bytes::from(header)));
            self.got_header = true;
        }

        while self.pending.len() >= 11 {
            let data_size = ((self.pending[1] as usize) << 16)
                | ((self.pending[2] as usize) << 8)
                | (self.pending[3] as usize);
            if data_size > MAX_FLV_TAG {
                // Corrupt/non-FLV framing: fall back to raw to avoid stalling.
                self.unparsable = true;
                let all = std::mem::take(&mut self.pending);
                out.push(FlvUnit::Raw(bytes::Bytes::from(all)));
                return out;
            }
            let total = 11 + data_size + 4;
            if self.pending.len() < total {
                break;
            }
            let tag_type = self.pending[0] & 0x1f;
            let tag: Vec<u8> = self.pending.drain(..total).collect();
            let is_keyframe = tag_type == 9
                && tag.len() > 11
                && (tag[11] >> 4) == 1
                // Exclude the AVC sequence header (frame type 1, codec 7, pkt 0).
                && !(tag.len() > 12 && tag[11] == 0x17 && tag[12] == 0x00);
            out.push(FlvUnit::Tag { bytes: bytes::Bytes::from(tag), is_keyframe });
        }
        if self.pending.len() > MAX_PENDING {
            self.unparsable = true;
            let all = std::mem::take(&mut self.pending);
            out.push(FlvUnit::Raw(bytes::Bytes::from(all)));
        }
        out
    }
}

/// Decoder-ready join point for clients that attach to an already running
/// stream. A DLNA renderer (TV) is not a browser: the browser player can
/// transmux a mid-stream start, while a TV fed only the file header and mid-GOP
/// frames keeps buffering and gives up. The primer replays
///   FLV header + PreviousTagSize0 + onMetaData + AVC/AAC sequence headers
///   + the latest video keyframe and the frames that followed it
/// before switching to live data, which is exactly what a renderer needs.
struct Primer {
    gen: u64,
    /// Total upstream bytes the parser has consumed (broadcast position), i.e.
    /// where a client handed this primer must resume reading.
    fed: u64,
    /// 9-byte FLV header + PreviousTagSize0, captured once per generation.
    head: Vec<u8>,
    /// onMetaData + codec sequence headers, captured once per generation.
    config: Vec<u8>,
    /// Latest keyframe tag plus every tag that followed it (one GOP).
    kf: Vec<u8>,
    /// Bytes the pump has broadcast so far (offset at the end of the primer).
    offset: u64,
    /// Incomplete tag bytes carried across chunks.
    pending: Vec<u8>,
    got_head: bool,
    got_meta: bool,
    got_vseq: bool,
    got_aseq: bool,
    /// Set when the payload is not FLV (mock/test sources): the primer then
    /// degrades to "head only" and never blocks the live path.
    unparsable: bool,
}

impl Default for Primer {
    fn default() -> Self {
        Self {
            gen: 0,
            fed: 0,
            head: Vec::new(),
            config: Vec::new(),
            kf: Vec::new(),
            offset: 0,
            pending: Vec::new(),
            got_head: false,
            got_meta: false,
            got_vseq: false,
            got_aseq: false,
            unparsable: false,
        }
    }
}

impl Primer {
    fn reset(&mut self, gen: u64) {
        *self = Primer { gen, fed: self.fed, offset: self.offset, ..Primer::default() };
    }

    /// Reconnect of the SAME source: keep the captured header/config/keyframe so
    /// late joiners stay primed across the gap, but drop any partial tag left
    /// from the previous session's EOF so it cannot corrupt the next parse.
    fn clear_pending(&mut self) {
        self.pending.clear();
        self.unparsable = false;
    }

    /// Consume a broadcast chunk, maintaining head/config/keyframe state.
    fn feed(&mut self, chunk: &[u8], gen: u64, offset_after: u64) {
        self.gen = gen;
        self.fed = offset_after;
        if self.unparsable {
            self.offset = offset_after;
            return;
        }
        self.pending.extend_from_slice(chunk);
        if self.pending.len() > MAX_PENDING {
            // Not FLV (or a pathological stream): stop trying, keep the head we
            // already captured so late joiners still get a valid file header.
            self.pending.clear();
            self.unparsable = true;
            return;
        }

        if !self.got_head {
            if self.pending.len() < 13 { return; }
            if &self.pending[..3] != b"FLV" {
                self.pending.clear();
                self.unparsable = true;
                return;
            }
            self.head.extend_from_slice(&self.pending[..9]);
            // PreviousTagSize0 refers to whatever we replay first, which is the
            // first real tag of the stream (a keyframe), so mirror its size.
            self.head.extend_from_slice(&[0, 0, 0, 0]);
            self.pending.drain(..13);
            self.got_head = true;
        }

        while self.pending.len() >= 11 {
            let tag_type = self.pending[0];
            let data_size = ((self.pending[1] as usize) << 16)
                | ((self.pending[2] as usize) << 8)
                | (self.pending[3] as usize);
            if data_size > MAX_FLV_TAG {
                self.pending.clear();
                self.unparsable = true;
                return;
            }
            let total = 11 + data_size + 4;
            if self.pending.len() < total {
                break;
            }
            let tag: Vec<u8> = self.pending[..total].to_vec();
            self.pending.drain(..total);
            let body = &tag[11..11 + data_size];
            self.offset = offset_after - self.pending.len() as u64;
            match tag_type {
                // video
                9 => {
                    let is_key = !body.is_empty() && (body[0] >> 4) == 1;
                    let is_seq = body.len() > 1 && body[0] == 0x17 && body[1] == 0x00;
                    if is_seq {
                        if !self.got_vseq {
                            self.got_vseq = true;
                            self.config.extend_from_slice(&tag);
                        }
                    } else if is_key {
                        self.kf.clear();
                        self.kf.extend_from_slice(&tag);
                    } else if !self.kf.is_empty() && self.kf.len() < MAX_KEYFRAME_BLOCK {
                        self.kf.extend_from_slice(&tag);
                    }
                }
                // audio: only the AAC sequence header belongs in the primer
                8 => {
                    let is_seq = body.len() > 1 && body[1] == 0x00;
                    if is_seq && !self.got_aseq {
                        self.got_aseq = true;
                        self.config.extend_from_slice(&tag);
                    }
                }
                // script data (onMetaData)
                18 => {
                    if !self.got_meta {
                        self.got_meta = true;
                        self.config.extend_from_slice(&tag);
                    }
                }
                _ => {}
            }
        }
    }

    /// Bytes to prepend for a client joining now, plus the pump offset they
    /// cover (so already-contained chunks are skipped instead of duplicated).
    fn snapshot(&self) -> Option<(bytes::Bytes, u64)> {
        if self.head.is_empty() { return None; }
        let mut out =
            Vec::with_capacity(self.head.len() + self.config.len() + self.kf.len() + self.pending.len());
        out.extend_from_slice(&self.head);
        out.extend_from_slice(&self.config);
        out.extend_from_slice(&self.kf);
        // The tail of the last chunk is only a partial FLV tag; it still has to
        // be replayed, otherwise the client resumes mid-tag and every renderer
        // downstream mis-parses the stream. `fed` is then the exact broadcast
        // position the live data continues from.
        out.extend_from_slice(&self.pending);
        Some((bytes::Bytes::from(out), self.fed))
    }
}

static PUMP: OnceLock<StdMutex<Option<(usize, Arc<SharedPump>)>>> = OnceLock::new();
static PUMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
    let (tx, _rx) = broadcast::channel::<(u64, u64, bytes::Bytes)>(4096);
    let id = PUMP_SEQ.fetch_add(1, Ordering::SeqCst) as u64 + 1;
    let p = Arc::new(SharedPump {
        tx,
        primer: StdMutex::new(Primer::default()),
        header_sent: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        task_running: AtomicBool::new(false),
        task_spawns: AtomicUsize::new(0),
        session_healthy: AtomicBool::new(false),
        broadcast_started: AtomicBool::new(false),
        id,
    });
    let mut slot = pump_slot().lock().unwrap();
    if let Some((old_key, old)) = slot.take() {
        println!("[proxy] pump #{} created (key={:#x}); cancelling pump #{} (key={:#x})", id, key, old.id, old_key);
        old.cancelled.store(true, Ordering::SeqCst);
    } else {
        println!("[proxy] pump #{} created (key={:#x})", id, key);
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
            // A room/quality switch is a legitimate proxy restart, not a failure:
            // it must not consume the respawn budget.
            p.task_spawns.store(0, Ordering::SeqCst);
            let gen = p.primer.lock().unwrap().gen;
            p.primer.lock().unwrap().reset(gen + 1);
            p.header_sent.store(false, Ordering::SeqCst);
            // The next upstream session is a brand new stream: clients must get
            // its real FLV header from the live path, not a stale primer.
            p.broadcast_started.store(false, Ordering::SeqCst);
        }
    }
}

/// Upper bound on pump task respawns for a single pump.
const MAX_PUMP_RESPAWNS: usize = 40;

fn ensure_pump(store: StreamUrlStore) {
    let p = pump_for(&store);
    if p.cancelled.load(Ordering::SeqCst) {
        return;
    }
    // EXACTLY ONE upstream task per pump. Douyu/Huya bind a signed URL to a
    // single TCP session: if every client connection spawned its own fetcher,
    // each new signature invalidated the others' session and the CDN dropped
    // them all within ~130ms (player + TV = broken cast).
    if p.task_running.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawns = p.task_spawns.fetch_add(1, Ordering::SeqCst);
    if spawns >= MAX_PUMP_RESPAWNS {
        eprintln!("[proxy] pump #{} hit the {} respawn cap; not restarting", p.id, MAX_PUMP_RESPAWNS);
        return;
    }
    if spawns > 0 {
        println!("[proxy] restarting pump #{} (respawn {})", p.id, spawns);
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

    let pump_id = p.id;
    tokio::spawn(async move {
        let tx = p.tx.clone();
        let mut attempts: u32 = 0;
        // Monotonic byte offset of the broadcast stream; primers reference it so
        // a joining client can skip chunks it already received.
        let mut offset_total: u64 = p.primer.lock().unwrap().offset;
        println!("[proxy] pump #{} task started", pump_id);
        // The actix server that hosts this task is recreated on every room
        // switch, which tears the task down; flag it so the next request
        // restarts the pump instead of serving an empty stream forever.
        struct Running<'a>(&'a AtomicBool);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _running_guard = Running(&p.task_running);
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
            let mut session_bytes: u64 = 0;
            let mut session_chunks: u64 = 0;
            match build_upstream_request(&client, &url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    attempts = 0;
                    let mut stream = resp.bytes_stream();
                    p.session_healthy.store(false, Ordering::SeqCst);
                    // Is this a brand new source (room/quality switch) or a
                    // reconnect of the same stream after a signature expired?
                    let new_source = !p.header_sent.load(Ordering::SeqCst);
                    // A reconnect must resume on a keyframe boundary with whole
                    // tags, or the decoder shows mosaic until the next GOP. A new
                    // source forwards everything (header + config + keyframe).
                    let mut await_keyframe = !new_source;
                    if new_source {
                        // Fresh stream: the primer rebuilds from this header.
                        p.primer.lock().unwrap().reset(gen_at_connect);
                    } else {
                        // Same stream: keep the primer's header/config/keyframe so
                        // late joiners stay primed across the reconnect gap.
                        p.primer.lock().unwrap().clear_pending();
                    }
                    let mut framer = FlvFramer::new();
                    while let Some(chunk) = stream.next().await {
                        if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                            println!("[proxy] stream source changed -> switch upstream");
                            break;
                        }
                        let bytes = match chunk {
                            Ok(b) => b,
                            Err(e) => {
                                eprintln!("[proxy] pump chunk error: {} (after {} chunks/{} bytes) -> reconnect", e, session_chunks, session_bytes);
                                break;
                            }
                        };
                        session_bytes += bytes.len() as u64;
                        session_chunks += 1;
                        if session_bytes > 200_000 && !p.session_healthy.swap(true, Ordering::SeqCst) {
                            // Genuinely streaming now: forget earlier respawn
                            // attempts so normal use is unlimited.
                            p.task_spawns.store(0, Ordering::SeqCst);
                        }

                        for unit in framer.push(&bytes) {
                            // Decide what to put on the wire. `forward` is the
                            // byte payload clients receive; it is always a whole
                            // tag (or the header), never a fragment.
                            let forward: Option<bytes::Bytes> = match unit {
                                FlvUnit::Header(h) => {
                                    // Only a new source broadcasts the header;
                                    // repeating it mid-stream corrupts attached
                                    // decoders.
                                    if new_source { Some(h) } else { None }
                                }
                                FlvUnit::Tag { bytes, is_keyframe, .. } => {
                                    if await_keyframe {
                                        if is_keyframe {
                                            await_keyframe = false;
                                            println!("[proxy] pump #{} resumed at keyframe after reconnect", pump_id);
                                            Some(bytes)
                                        } else {
                                            // Drop mid-GOP tags until the first keyframe.
                                            None
                                        }
                                    } else {
                                        Some(bytes)
                                    }
                                }
                                FlvUnit::Raw(b) => {
                                    // Non-FLV (tests/mocks): on a reconnect we
                                    // cannot realign, so forward from the start.
                                    await_keyframe = false;
                                    Some(b)
                                }
                            };

                            let Some(payload) = forward else { continue };
                            let offset_before = offset_total;
                            offset_total += payload.len() as u64;
                            // Feed the primer EXACTLY what goes on the wire so its
                            // offsets stay aligned with the broadcast offsets used
                            // for late-join de-duplication.
                            p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                            p.header_sent.store(true, Ordering::SeqCst);
                            p.broadcast_started.store(true, Ordering::SeqCst);
                            let _ = tx.send((gen_at_connect, offset_before, payload));
                        }
                    }
                    println!(
                        "[proxy] pump #{} upstream EOF after {} chunks/{} bytes, {} clients, lived {:?} -> refresh URL and reconnect",
                        pump_id,
                        session_chunks,
                        session_bytes,
                        CLIENT_COUNT.load(Ordering::Relaxed),
                        connected_at.elapsed()
                    );
                }
                Ok(resp) => {
                    eprintln!("[proxy] pump upstream status {} -> retry", resp.status());
                    rejected = true;
                }
                Err(e) => eprintln!("[proxy] pump connect error: {} -> retry", e),
            }

            // If the source changed (room/quality switch), drop the primer and
            // header state so the new stream's real FLV header and codec config
            // are broadcast once to every client.
            if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                let mut pr = p.primer.lock().unwrap();
                pr.reset(gen_at_connect);
                drop(pr);
                p.header_sent.store(false, Ordering::SeqCst);
                p.broadcast_started.store(false, Ordering::SeqCst);
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
                "[proxy] pump #{} re-signing upstream (rejected={} lived={:?} cancelled={})",
                pump_id, rejected, lived, p.cancelled.load(Ordering::SeqCst)
            );
            refresh_store_url(&store, gen_at_connect, &p.cancelled).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
    });
}

pub static CLIENT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ClientStream {
    inner: tokio_stream::wrappers::BroadcastStream<(u64, u64, bytes::Bytes)>,
    /// Primer (FLV head + codec config + latest keyframe) for a client that
    /// joined after the stream started; `None` when it can read from the stream
    /// start instead.
    primer: Option<bytes::Bytes>,
    /// Broadcast offset covered by `primer`: chunks ending at or before it are
    /// skipped so nothing is delivered twice.
    primer_offset: u64,
    expect_gen: Option<u64>,
}

impl Drop for ClientStream {
    fn drop(&mut self) {
        CLIENT_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

impl futures_util::Stream for ClientStream {
    type Item = Result<bytes::Bytes, actix_web::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(h) = self.primer.take() {
            if !h.is_empty() {
                return std::task::Poll::Ready(Some(Ok(h)));
            }
        }
        loop {
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok((gen, offset, bytes)))) => {
                    match self.expect_gen {
                        None => self.expect_gen = Some(gen),
                        Some(g) if g != gen => {
                            // Source switched mid-stream: end this response so the
                            // client reconnects cleanly to the new stream.
                            return std::task::Poll::Ready(None);
                        }
                        Some(_) => {}
                    }
                    let end = offset + bytes.len() as u64;
                    if end <= self.primer_offset {
                        // Already contained in the primer sent above.
                        continue;
                    }
                    if offset < self.primer_offset {
                        // The primer ended inside this chunk: hand over only the
                        // part it does not already contain (no gap, no overlap).
                        let skip = (self.primer_offset - offset) as usize;
                        return std::task::Poll::Ready(Some(Ok(bytes.slice(skip..))));
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
    req: HttpRequest,
    stream_url_store: web::Data<StreamUrlStore>,
) -> impl Responder {
    // Renderers (DLNA TVs) send hints about what they expect: Range, DLNA
    // feature probes, user agent. Logging them is what revealed why a TV would
    // buffer forever and give up.
    {
        let h = req.headers();
        let mut parts: Vec<String> = Vec::new();
        for (k, v) in h.iter() {
            let name = k.as_str();
            if matches!(name, "host" | "connection" | "accept-encoding") { continue; }
            parts.push(format!("{}={}", name, v.to_str().unwrap_or("?")));
        }
        println!(
            "[proxy] client request from {:?} method={} headers: {}",
            req.peer_addr(),
            req.method(),
            parts.join(" ")
        );
    }
    let url = stream_url_store.url.lock().unwrap().clone();
    if url.is_empty() {
        return HttpResponse::NotFound().body("Stream URL is not set or empty.");
    }
    println!("[Rust/proxy.rs handler] Incoming FLV proxy request -> {}", url);

    let store: StreamUrlStore = (**stream_url_store).clone();
    ensure_pump(store.clone());

    let p = pump_for(&store);
    let n = CLIENT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let current_gen = store.generation.load(Ordering::SeqCst);
    // Subscribe BEFORE reading the primer so no byte can slip through the gap:
    // chunks that the primer already covers are skipped by offset below.
    let rx = p.tx.subscribe();
    let (primer, primer_offset) = {
        let pr = p.primer.lock().unwrap();
        // Always replay what we have: a client attaching before the FLV header
        // arrives simply gets no primer and reads the stream from its start.
        {
            if pr.gen == current_gen {
                match pr.snapshot() {
                Some((b, off)) => {
                    println!(
                        "[proxy] client #{} late join (gen={}, pump #{}): primer {} bytes @ offset {}",
                        n, current_gen, p.id, b.len(), off
                    );
                    (Some(b), off)
                }
                None => {
                    println!("[proxy] client #{} joined (gen={}, pump #{}): stream from start", n, current_gen, p.id);
                    (None, 0)
                }
            }
            } else {
                println!("[proxy] client #{} joined (gen={}, pump #{}): primer gen mismatch", n, current_gen, p.id);
                (None, 0)
            }
        }
    };
    HttpResponse::Ok()
        .content_type("video/x-flv")
        .insert_header(("Connection", "keep-alive"))
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("Accept-Ranges", "bytes"))
        .streaming(ClientStream {
            inner: tokio_stream::wrappers::BroadcastStream::new(rx),
            primer,
            primer_offset,
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

    fn flv_tag(tag_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(tag_type);
        let n = body.len() as u32;
        v.push((n >> 16) as u8);
        v.push((n >> 8) as u8);
        v.push(n as u8);
        v.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // timestamp(3)+ext(1)+streamid(3)
        v.extend_from_slice(body);
        let size = 11 + body.len();
        v.extend_from_slice(&[((size >> 24) & 0xff) as u8, ((size >> 16) & 0xff) as u8, ((size >> 8) & 0xff) as u8, (size & 0xff) as u8]);
        v
    }

    /// A renderer joining mid-stream must be handed a decoder-ready primer:
    /// FLV head + onMetaData + sequence headers + the latest keyframe, and the
    /// keyframe block must be replaced (not appended) when a new one arrives.
    #[test]
    fn primer_replays_config_and_latest_keyframe() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"FLV\x01\x05\x00\x00\x00\x09");
        stream.extend_from_slice(&[0, 0, 0, 0]); // PreviousTagSize0
        let meta = flv_tag(18, b"\x02\x00\x0aonMetaData");
        let vseq = flv_tag(9, &[0x17, 0x00, 0x00, 0x00, 0x00, 0xAA, 0xBB]);
        let aseq = flv_tag(8, &[0xAF, 0x00, 0x12, 0x10]);
        let key1 = flv_tag(9, &[0x17, 0x01, 0x00, 0x00, 0x00, 0x11, 0x22]);
        let delta1 = flv_tag(9, &[0x27, 0x01, 0x00, 0x00, 0x00, 0x33, 0x44]);
        let audio1 = flv_tag(8, &[0xAF, 0x01, 0xDE, 0xAD]);
        let key2 = flv_tag(9, &[0x17, 0x01, 0x00, 0x00, 0x00, 0x55, 0x66]);
        let delta2 = flv_tag(9, &[0x27, 0x01, 0x00, 0x00, 0x00, 0x77, 0x88]);
        stream.extend_from_slice(&meta);
        stream.extend_from_slice(&vseq);
        stream.extend_from_slice(&aseq);
        stream.extend_from_slice(&key1);
        stream.extend_from_slice(&delta1);
        stream.extend_from_slice(&audio1);
        stream.extend_from_slice(&key2);
        stream.extend_from_slice(&delta2);

        let mut pr = Primer::default();
        // Feed in awkward small pieces so tags straddle chunk boundaries.
        let mut i = 0;
        while i < stream.len() {
            let end = (i + 7).min(stream.len());
            pr.feed(&stream[i..end], 3, end as u64);
            i = end;
        }

        let (bytes, off) = pr.snapshot().expect("primer must exist");
        assert!(bytes.starts_with(b"FLV"), "primer must start with the FLV header");
        assert_eq!(off, stream.len() as u64, "primer offset must cover everything fed");
        assert!(bytes.windows(10).any(|w| w == b"onMetaData"), "metadata must be replayed");
        // sequence headers appear exactly once
        assert!(bytes.windows(2).filter(|w| *w == [0x17, 0x00]).count() == 1, "video seq header once");
        assert!(bytes.windows(2).any(|w| *w == [0xAF, 0x00]), "audio seq header present");
        // only the LATEST keyframe and what followed it
        assert!(bytes.windows(6).any(|w| *w == [0x17, 0x01, 0, 0, 0, 0x55]), "key2 present");
        assert!(bytes.windows(6).any(|w| *w == [0x27, 0x01, 0, 0, 0, 0x77]), "delta2 present");
        assert!(!bytes.windows(6).any(|w| *w == [0x17, 0x01, 0, 0, 0, 0x11]), "stale key1 must be dropped");
        assert!(!bytes.windows(6).any(|w| *w == [0x27, 0x01, 0, 0, 0, 0x33]), "stale delta1 must be dropped");
        assert!(!bytes.windows(4).any(|w| *w == [0xAF, 0x01, 0xDE, 0xAD]), "mid-GOP audio must not bloat the primer");

        // Every tag in the primer must carry its trailing PreviousTagSize so the
        // bytes concatenate into a valid FLV.
        let mut pos = 9 + 4;
        while pos + 11 <= bytes.len() {
            let t = bytes[pos];
            let sz = ((bytes[pos + 1] as usize) << 16) | ((bytes[pos + 2] as usize) << 8) | (bytes[pos + 3] as usize);
            assert!(matches!(t, 8 | 9 | 18), "unexpected tag type {}", t);
            assert!(pos + 11 + sz + 4 <= bytes.len(), "truncated tag at {}", pos);
            pos += 11 + sz + 4;
        }
        assert_eq!(pos, bytes.len(), "primer must consist of whole tags only");
    }

    /// Non-FLV payloads (mock CDNs, tests) must never wedge the primer; a later
    /// real FLV header from a reconnect is still captured.
    #[test]
    fn primer_degrades_gracefully_on_garbage() {
        let mut pr = Primer::default();
        pr.feed(b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00", 0, 13);
        // A tag claiming an impossibly large body: parser must give up safely.
        pr.feed(&[9, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4], 0, 28);
        assert!(pr.unparsable, "malformed tag must mark the primer unparsable");
        let (bytes, _) = pr.snapshot().expect("head-only primer still useful");
        assert!(bytes.starts_with(b"FLV"));
    }

    /// Walk an FLV byte buffer verifying that every tag header matches its
    /// trailing PreviousTagSize, i.e. the stream is structurally continuous.
    /// Returns how many bytes were validated.
    fn validated_flv_len(b: &[u8]) -> usize {
        if b.len() < 13 || &b[..3] != b"FLV" { return 0; }
        let mut pos = 13usize;
        let mut prev_size: usize = 0;
        while pos + 11 <= b.len() {
            let size = ((b[pos + 1] as usize) << 16) | ((b[pos + 2] as usize) << 8) | (b[pos + 3] as usize);
            if pos + 11 + size + 4 > b.len() { break; }
            let tail = &b[pos + 11 + size..pos + 11 + size + 4];
            let tail_size = ((tail[0] as usize) << 24) | ((tail[1] as usize) << 16)
                | ((tail[2] as usize) << 8) | (tail[3] as usize);
            // PreviousTagSizeN must equal 11 + data size of tag N-1.
            if prev_size != 0 && tail_size != 11 + size { return 0; }
            prev_size = 11 + size;
            pos += 11 + size + 4;
        }
        let _ = prev_size;
        pos
    }

    /// Build a structurally valid FLV: header, metadata, sequence headers, then
    /// keyframes with deltas, sized so tags straddle chunk boundaries.
    /// Returns the stream plus the offset right after the head/metadata/sequence
    /// headers, i.e. where a live re-send would loop without repeating config.
    fn synthetic_flv(keyframes: usize, delta_size: usize) -> (Vec<u8>, usize) {
        // flv_tag lives in this test module
        let mut v = Vec::new();
        v.extend_from_slice(b"FLV\x01\x05\x00\x00\x00\x09");
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(&flv_tag(18, b"\x02\x00\x0aonMetaData\x00\x08\x00\x00\x00\x01\x00\x09\x00\x05"));
        v.extend_from_slice(&flv_tag(9, &[0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x00]));
        v.extend_from_slice(&flv_tag(8, &[0xAF, 0x00, 0x12, 0x10]));
        let config_end = v.len();
        for k in 0..keyframes {
            v.extend_from_slice(&flv_tag(9, &[0x17, 0x01, 0x00, 0x00, (k as u8), 0x65, 0x87]));
            for d in 0..6 {
                // Unique fill per tag so a resumed stream can be located exactly.
                let fill = ((k * 16 + d) & 0xFF) as u8;
                let mut body = vec![0x27, 0x01, 0x00, 0x00, d as u8];
                body.resize(delta_size, fill);
                v.extend_from_slice(&flv_tag(9, &body));
            }
        }
        (v, config_end)
    }

    /// A renderer (or any client) that attaches mid-stream must receive a
    /// structurally continuous FLV: the primer replays the head, metadata,
    /// sequence headers and the latest keyframe, then live data continues with
    /// neither a gap nor a duplicated tag.
    #[actix_web::test]
    async fn late_client_primer_is_structurally_continuous() {
        let (full, config_end) = synthetic_flv(40, 4096);
        let chunk_len = 7777; // deliberately misaligned with tag boundaries
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let full_for_cdn = full.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let data = full_for_cdn.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nTransfer-Encoding: chunked\r\n\r\n").await;
                    let mut i = 0usize;
                    loop {
                        let end = (i + chunk_len).min(data.len());
                        if i >= end {
                            // Loop like a real live stream: continue with payload
                            // tags, never re-send the head/codec configuration.
                            i = config_end;
                            continue;
                        }
                        let part = &data[i..end];
                        let _ = sock.write_all(format!("{:x}\r\n", part.len()).as_bytes()).await;
                        let _ = sock.write_all(part).await;
                        let _ = sock.write_all(b"\r\n").await;
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        i = end;
                    }
                });
            }
        });

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", port);
        // Unique port: the pump slot is process-global, so two tests sharing a
        // port also share (and cancel) each other's pump.
        spawn_test_server(store, 39931).await;
        let client = Client::builder().no_proxy().build().unwrap();

        // First client starts the pump and consumes most of the stream.
        let r1 = client.get("http://127.0.0.1:39931/live.flv").send().await.unwrap();
        let mut s1 = r1.bytes_stream();
        let mut early = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while early.len() < 200_000 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s1.next()).await {
                Ok(Some(Ok(b))) => early.extend_from_slice(&b),
                _ => break,
            }
        }
        assert!(early.starts_with(b"FLV"));

        // Late client joins mid-stream.
        let r2 = client.get("http://127.0.0.1:39931/live.flv").send().await.unwrap();
        let mut s2 = r2.bytes_stream();
        let mut late = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while late.len() < 260_000 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s2.next()).await {
                Ok(Some(Ok(b))) => late.extend_from_slice(&b),
                Ok(None) => break,
                _ => break,
            }
        }
        assert!(late.starts_with(b"FLV"), "late client must start with an FLV header");
        // sequence headers replayed exactly once, at the start
        let vseq = [0x17u8, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42];
        assert_eq!(late.windows(vseq.len()).filter(|w| *w == vseq).count(), 1,
            "video sequence header must be replayed exactly once");
        assert!(late.windows(10).any(|w| w == b"onMetaData"), "metadata must be replayed");
        // structurally continuous: primer + live must parse as whole tags
        let ok = validated_flv_len(&late);
        if ok <= 200_000 {
            // Structural break: locate where the live continuation resumed by
            // matching the unique per-tag fill byte against the source stream.
            let after = &late[ok..(ok + 64).min(late.len())];
            println!("[primer] break at {}; next 32 bytes: {}", ok,
                after[..32.min(after.len())].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" "));
            for window in 13..(full.len().saturating_sub(after.len().max(8))) {
                if full[window..].starts_with(&after[..after.len().min(8)]) {
                    println!("[primer] continuation matches source at offset {}", window);
                    break;
                }
            }
            println!("[primer] primer end (len) = {}; expect continuation right after it", ok);
        }
        assert!(ok > 200_000, "late stream only validated {} of {} bytes (gap/overlap in primer join)", ok, late.len());
        println!("[primer] late client got {} bytes, {} validated as continuous FLV", late.len(), ok);
    }

    /// The exact production failure this guards: a signed URL expires (~every
    /// 300s for Douyu), the CDN drops the connection mid-FLV-tag, and the pump
    /// re-signs and reconnects to a *fresh* session that does not necessarily
    /// start at a keyframe. Without whole-tag framing plus keyframe realignment
    /// the pump would replay a torn half-tag and then mid-GOP delta frames, and
    /// every already-watching player shows mosaic until its next keyframe --
    /// "看着看着总有马赛克，一会儿就好了，过一段时间还有".
    #[actix_web::test]
    async fn reconnect_after_signature_expiry_never_shows_partial_or_mid_gop_frames() {
        use std::sync::atomic::AtomicUsize;

        // Conn #0 streams valid FLV and then abruptly closes mid-tag (fill
        // marker 0xEE marks the torn, never-to-be-forwarded bytes). Conn #1+
        // starts fresh: config, then deliberately NON-keyframe delta tags
        // (marker 0x22) before its first real keyframe (marker 0x33). If the
        // fix regresses, 0xEE or 0x22 would leak to an already-attached client.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns_task = conns.clone();

        async fn write_chunk(sock: &mut tokio::net::TcpStream, data: &[u8]) {
            use tokio::io::AsyncWriteExt;
            let framed = format!("{:x}\r\n", data.len());
            let _ = sock.write_all(framed.as_bytes()).await;
            let _ = sock.write_all(data).await;
            let _ = sock.write_all(b"\r\n").await;
        }

        let flv_header = || -> Vec<u8> {
            let mut v = b"FLV\x01\x05\x00\x00\x00\x09".to_vec();
            v.extend_from_slice(&[0, 0, 0, 0]);
            v
        };
        let meta_tag = || flv_tag(18, b"\x02\x00\x0aonMetaData");
        let vseq_tag = || flv_tag(9, &[0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42]);
        let aseq_tag = || flv_tag(8, &[0xAF, 0x00, 0x12, 0x10]);
        let filled_tag = |body_prefix: &[u8], fill: u8, size: usize| -> Vec<u8> {
            let mut body = body_prefix.to_vec();
            body.resize(size, fill);
            flv_tag(9, &body)
        };

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let n = conns_task.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(
                        &mut sock,
                        b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nTransfer-Encoding: chunked\r\n\r\n",
                    )
                    .await;

                    write_chunk(&mut sock, &flv_header()).await;
                    write_chunk(&mut sock, &meta_tag()).await;
                    write_chunk(&mut sock, &vseq_tag()).await;
                    write_chunk(&mut sock, &aseq_tag()).await;

                    if n == 0 {
                        // One real keyframe + a delta (both forwarded normally).
                        write_chunk(&mut sock, &filled_tag(&[0x17, 0x01, 0, 0, 0], 0x11, 4096)).await;
                        write_chunk(&mut sock, &filled_tag(&[0x27, 0x01, 0, 0, 0], 0x11, 4096)).await;
                        // Then a THIRD tag that gets torn mid-body: only its
                        // header + a few body bytes are ever written before the
                        // socket drops, exactly like a CDN cutting a session.
                        let torn = filled_tag(&[0x27, 0x01, 0, 0, 0], 0xEE, 4096);
                        write_chunk(&mut sock, &torn[..14]).await;
                        return; // drop sock -> EOF for the pump
                    }

                    // Fresh session: config, then tags that must NOT be forwarded
                    // until a real keyframe arrives.
                    write_chunk(&mut sock, &filled_tag(&[0x27, 0x01, 0, 0, 0], 0x22, 2048)).await;
                    write_chunk(&mut sock, &filled_tag(&[0x27, 0x01, 0, 0, 0], 0x22, 2048)).await;
                    write_chunk(&mut sock, &flv_tag(8, &[0xAF, 0x01, 0x22, 0x22])).await;
                    // The first real keyframe of this session: forwarding resumes here.
                    write_chunk(&mut sock, &filled_tag(&[0x17, 0x01, 0, 0, 0], 0x33, 4096)).await;
                    // Then keep streaming normally forever so the test client can
                    // read a stable amount without racing the connection close.
                    loop {
                        write_chunk(&mut sock, &filled_tag(&[0x27, 0x01, 0, 0, 0], 0x33, 4096)).await;
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                });
            }
        });

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", port);
        // Unique port: the pump slot is process-global; a shared port would let
        // two tests cancel each other's pump.
        spawn_test_server(store, 39932).await;
        let client = Client::builder().no_proxy().build().unwrap();

        // One client stays attached across the whole mock expiry/reconnect cycle.
        let resp = client.get("http://127.0.0.1:39932/live.flv").send().await.unwrap();
        let mut s = resp.bytes_stream();
        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while collected.len() < 150_000 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(800), s.next()).await {
                Ok(Some(Ok(b))) => collected.extend_from_slice(&b),
                Ok(Some(Err(e))) => panic!("client stream error: {}", e),
                Ok(None) => break,
                Err(_) => continue,
            }
        }
        assert!(collected.starts_with(b"FLV"), "client stream must start with FLV header");
        assert!(conns.load(Ordering::SeqCst) >= 2, "mock CDN must have been reconnected to");

        // 1) The torn, half-written tag from conn #0 must never reach a client.
        assert!(
            !collected.contains(&0xEE),
            "a partial tag torn mid-EOF was forwarded to the client (would decode as garbage/mosaic)"
        );
        // 2) Pre-keyframe (mid-GOP) frames from the fresh session must be held
        //    back until a real keyframe arrives.
        assert!(
            !collected.contains(&0x22),
            "non-keyframe tags from the reconnected session were forwarded before a keyframe (causes mosaic)"
        );
        // 3) The keyframe itself must have made it through.
        assert!(collected.contains(&0x33), "reconnected session's keyframe never reached the client");
        // 4) The FLV file header must appear exactly once: a reconnect must not
        //    rebroadcast it mid-stream (that alone desyncs every decoder).
        assert_eq!(
            collected.windows(3).filter(|w| *w == b"FLV").count(),
            1,
            "FLV header was rebroadcast after reconnect"
        );
        // 5) Everything the client received must parse as structurally
        //    continuous FLV, right up to and past the resume point.
        let walk = |b: &[u8]| -> Vec<(usize, u8, Vec<u8>)> {
            let mut out = Vec::new();
            if b.len() < 13 || &b[..3] != b"FLV" { return out; }
            let mut pos = 13usize;
            while pos + 11 <= b.len() {
                let tag_type = b[pos] & 0x1f;
                let size = ((b[pos + 1] as usize) << 16)
                    | ((b[pos + 2] as usize) << 8)
                    | (b[pos + 3] as usize);
                if pos + 11 + size + 4 > b.len() { break; }
                let body = b[pos + 11..pos + 11 + size].to_vec();
                out.push((pos, tag_type, body));
                pos += 11 + size + 4;
            }
            out
        };
        let tags = walk(&collected);
        assert!(!tags.is_empty(), "no complete tags parsed from what the client received");
        let resume_idx = tags
            .iter()
            .position(|(_, _, body)| body.contains(&0x33))
            .expect("keyframe marker tag must be present in parsed tags");
        let (_, tag_type, body) = &tags[resume_idx];
        assert_eq!(*tag_type, 9, "resume tag must be a video tag");
        assert!(
            (body.first().copied().unwrap_or(0) >> 4) == 1,
            "resumed tag is not a keyframe (frame type {})",
            body.first().copied().unwrap_or(0) >> 4
        );
        assert_eq!(
            resume_idx,
            tags.iter().position(|t| t == &tags[resume_idx]).unwrap(),
            "sanity: resume tag index lookup"
        );
        let resume_pos = tags[resume_idx].0;
        let validated = validated_flv_len(&collected);
        assert!(
            validated > resume_pos,
            "structural FLV validation stopped at {} before reaching the resume tag at {}",
            validated,
            resume_pos
        );
        println!(
            "[reconnect] client got {} bytes, resumed cleanly at tag #{} (offset {}), validated {} bytes",
            collected.len(),
            resume_idx,
            resume_pos,
            validated
        );
    }

    /// Builds a server exactly like `start_proxy_inner` does (same handler, same
    /// app data) on a test port, so a test can recreate it and reproduce the
    /// room-switch path the frontend takes.
    fn build_test_proxy(store: StreamUrlStore, port: u16) -> actix_web::dev::Server {
        HttpServer::new(move || {
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
        .run()
    }

    /// The frontend recreates the proxy server on every room switch (stop_proxy +
    /// start_proxy). That teardown aborts the pump task spawned on the server's
    /// runtime, so the next request must restart it - otherwise every client gets
    /// HTTP 200 with zero bytes forever (player stuck, TV never starts).
    #[actix_web::test]
    async fn proxy_restart_after_room_switch_keeps_streaming() {
        let cdn_a = mock_cdn_tagged(0xAA).await;
        let cdn_b = mock_cdn_tagged(0xBB).await;
        let port = 39934;
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_a);

        let srv = build_test_proxy(store.clone(), port);
        let srv_handle = srv.handle();
        actix_web::rt::spawn(srv);
        tokio::time::sleep(Duration::from_millis(400)).await;

        let client = Client::builder().no_proxy().build().unwrap();
        async fn read_some(
            client: &Client, port: u16, want: u8, forbid: u8, label: &str,
        ) -> Vec<u8> {
            let url = format!("http://127.0.0.1:{}/live.flv", port);
            let resp = client.get(&url).send().await.expect("connect");
            assert!(resp.status().is_success(), "{} status", label);
            let mut st = resp.bytes_stream();
            let mut got = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            while got.len() < 24_000 && tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_millis(700), st.next()).await {
                    Ok(Some(Ok(b))) => got.extend_from_slice(&b),
                    Ok(Some(Err(e))) => panic!("{} stream error: {}", label, e),
                    Ok(None) => break,
                    Err(_) => continue,
                }
            }
            assert!(got.starts_with(b"FLV"), "{} must start with an FLV header", label);
            assert!(got.contains(&want), "{} missing expected payload 0x{:02x}", label, want);
            assert!(!got.contains(&forbid), "{} leaked previous room payload 0x{:02x}", label, forbid);
            got
        }

        let a = read_some(&client, port, 0xAA, 0xBB, "room A").await;
        println!("[restart] room A read {} bytes", a.len());

        // Frontend room switch: stop the proxy, point at room B, bump the
        // generation, reset the primer, then start the proxy again.
        srv_handle.stop(false).await;
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_b);
        store.generation.fetch_add(1, Ordering::SeqCst);
        reset_pump_header();
        let srv2 = build_test_proxy(store.clone(), port);
        actix_web::rt::spawn(srv2);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let b = read_some(&client, port, 0xBB, 0xAA, "room B after restart").await;
        println!("[restart] room B read {} bytes after server restart", b.len());

        // A renderer joining later must still be primed with a decoder-ready start.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let c = read_some(&client, port, 0xBB, 0xAA, "late joiner after restart").await;
        println!("[restart] late joiner read {} bytes (primed)", c.len());
        assert!(c.len() > 20_000, "late joiner got too little data: {}", c.len());
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

    /// Long-running (real network) check for the fix that stops mosaic on
    /// reconnect: hold ONE client connection open across at least one natural
    /// Douyu signature expiry (~300s) and verify the bytes it received are one
    /// structurally continuous FLV -- no torn tag forwarded from a session that
    /// was cut mid-frame, and no mid-GOP delta frames forwarded before the next
    /// real keyframe after the re-sign reconnect.
    ///
    /// Ignored by default: this runs ~7 minutes against a real, live room.
    /// Override the room/quality/duration with env vars:
    ///   DTV_TEST_ROOM, DTV_TEST_QUALITY, DTV_LONGRUN_SECS
    #[actix_web::test]
    #[ignore = "long live network test (~7min): real Douyu signature expiry + reconnect"]
    async fn live_douyu_reconnect_across_expiry_no_torn_tags() {
        let room = std::env::var("DTV_TEST_ROOM").unwrap_or_else(|_| "9999".to_string());
        let quality = std::env::var("DTV_TEST_QUALITY").unwrap_or_else(|_| "原画".to_string());
        let duration: u64 = std::env::var("DTV_LONGRUN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(420);

        let url = crate::platforms::douyu::get_stream_url_with_quality(&room, &quality, None)
            .await
            .expect("failed to get douyu stream url");
        println!("[longrun] room={} quality={} duration={}s", room, quality, duration);

        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = url;
        *store.platform.lock().unwrap() = Some("douyu".into());
        *store.room_id.lock().unwrap() = Some(room.clone());
        *store.quality.lock().unwrap() = Some(quality);
        let server_handle = Arc::new(StdMutex::new(None));
        let local_url = start_proxy_inner(server_handle.clone(), store.clone())
            .await
            .expect("proxy start");
        println!("[longrun] proxy ready at {}", local_url);

        let client = Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let resp = client.get(&local_url).send().await.expect("connect");
        assert_eq!(resp.status(), 200, "proxy status");
        let mut stream = resp.bytes_stream();

        let mut collected: Vec<u8> = Vec::new();
        let start = std::time::Instant::now();
        let deadline = start + Duration::from_secs(duration);
        let mut last_report = start;
        let mut last_total: usize = 0;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(b))) => collected.extend_from_slice(&b),
                Ok(Some(Err(e))) => panic!("stream error at {}s after {} bytes: {:#}",
                    start.elapsed().as_secs(), collected.len(), e),
                Ok(None) => panic!("stream EOF at {}s after {} bytes",
                    start.elapsed().as_secs(), collected.len()),
                Err(_) => continue,
            }
            if last_report.elapsed() >= Duration::from_secs(60) {
                let delta = collected.len() - last_total;
                println!("[longrun] t={}s total={} bytes (+{} in last 60s)",
                    start.elapsed().as_secs(), collected.len(), delta);
                last_report = std::time::Instant::now();
                last_total = collected.len();
            }
        }
        println!("[longrun] finished, total {} bytes over {}s",
            collected.len(), start.elapsed().as_secs());
        assert!(collected.starts_with(b"FLV"), "must start with an FLV header");

        // Walk the whole buffer as FLV tags; a torn/partial tag anywhere would
        // break this walk early (same check that caught the mock regression).
        let mut pos = 13usize; // past the 9-byte FLV header + PreviousTagSize0
        let mut tag_count = 0usize;
        let mut first_break: Option<usize> = None;
        while pos + 11 <= collected.len() {
            let size = ((collected[pos + 1] as usize) << 16)
                | ((collected[pos + 2] as usize) << 8)
                | (collected[pos + 3] as usize);
            if size > MAX_FLV_TAG { first_break = Some(pos); break; }
            if pos + 11 + size + 4 > collected.len() {
                // Only the very last, still-in-flight tag may be incomplete.
                break;
            }
            let tail = &collected[pos + 11 + size..pos + 11 + size + 4];
            let tail_size = ((tail[0] as usize) << 24) | ((tail[1] as usize) << 16)
                | ((tail[2] as usize) << 8) | (tail[3] as usize);
            if tag_count > 0 && tail_size != 11 + size {
                first_break = Some(pos);
                break;
            }
            tag_count += 1;
            pos += 11 + size + 4;
        }
        let leftover = collected.len() - pos;
        println!("[longrun] parsed {} whole tags; {} trailing bytes (in-flight tag at cutoff)",
            tag_count, leftover);
        if let Some(break_at) = first_break {
            let window = &collected[break_at.saturating_sub(8)..(break_at + 24).min(collected.len())];
            panic!(
                "FLV structure broke at byte {} (after {} whole tags): {} - a torn/mid-tag forward leaked to the client",
                break_at, tag_count,
                window.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
            );
        }
        assert!(leftover < 200_000, "too many trailing bytes ({}) to be just one in-flight tag", leftover);
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

    /// Exercises the exact commands the cast dialog calls, in order:
    ///   discover_dlna_devices -> start_proxy (0.0.0.0:34719) ->
    ///   get_cast_info's lan_url -> push_to_dlna -> TV must report PLAYING.
    /// Requires a real renderer on the LAN (no env vars needed).
    #[actix_web::test]
    #[ignore = "live network test: requires a reachable DLNA renderer"]
    async fn live_cast_flow_uses_app_commands() {
        let _ = env_logger::builder().is_test(true).try_init();

        let started = std::time::Instant::now();
        let devices = crate::dlna::discover_dlna_devices()
            .await
            .expect("discovery must succeed on a LAN with a renderer");
        println!("[castflow] discovered {} device(s) in {:.1}s", devices.len(), started.elapsed().as_secs_f32());
        for d in &devices {
            println!("  - {} @ {} ctrl={}", d.name, d.host, d.control_url);
        }
        // Prefer the renderer whose control URL is the classic UPnP one; a
        // bilibili-helper renderer on the same TV is not a reliable target.
        let dev = devices
            .iter()
            .find(|d| d.control_url.contains("AVTransport_control"))
            .or_else(|| devices.iter().find(|d| !d.control_url.contains("bilibili")))
            .unwrap_or(&devices[0])
            .clone();
        println!("[castflow] casting to {} ({})", dev.name, dev.control_url);
        assert!(started.elapsed().as_secs() < 20, "discovery too slow: {:.1}s", started.elapsed().as_secs_f32());

        // Real Douyu stream through the app's own proxy command.
        let room = std::env::var("DTV_TEST_ROOM").unwrap_or_else(|_| "9999".to_string());
        let url = crate::platforms::douyu::get_stream_url(&room, None)
            .await
            .expect("douyu stream url");
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = url;
        *store.platform.lock().unwrap() = Some("douyu".into());
        *store.room_id.lock().unwrap() = Some(room);
        *store.quality.lock().unwrap() = Some("原画".into());
        let handle = ProxyServerHandle::default();
        let local_url = start_proxy_inner(handle.0.clone(), store.clone())
            .await
            .expect("proxy start");
        println!("[castflow] proxy at {}", local_url);

        // Exactly what get_cast_info hands to the renderer.
        let ip = local_ip_address::local_ip().expect("lan ip");
        let lan_url = format!("http://{}:34719/live.flv", ip);
        println!("[castflow] cast url {}", lan_url);

        let client = Client::builder()
            .no_proxy()
            // No whole-request timeout: reqwest's `timeout` also aborts the
            // response body, which would break a live stream soak.
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let resp = client.get(&lan_url).send().await.expect("cast url connect");
        assert_eq!(resp.status(), 200, "cast url status");
        let mut stream = resp.bytes_stream();
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while got.len() < 300_000 && std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(b))) => got.extend_from_slice(&b),
                Ok(Some(Err(e))) => panic!("cast url stream error: {:#}", e),
                Ok(None) => panic!("cast url EOF at {} bytes", got.len()),
                Err(_) => continue,
            }
        }
        assert!(got.starts_with(b"FLV"), "cast url must serve FLV");
        assert!(got.len() >= 200_000, "cast url served only {} bytes", got.len());
        println!("[castflow] cast url served {} bytes and starts with FLV", got.len());

        // Optional diagnostic: soak the pump for a while WITHOUT pushing to the
        // renderer, to tell "pump unhealthy" apart from "renderer broke it".
        if let Ok(secs) = std::env::var("DTV_CAST_SOAK_SECS") {
            let secs: u64 = secs.parse().unwrap_or(0);
            println!("[castflow] soaking cast url for {}s without pushing", secs);
            let mut total = got.len();
            let soak_end = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < soak_end {
                match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                    Ok(Some(Ok(b))) => total += b.len(),
                    Ok(Some(Err(e))) => panic!("soak stream error after {} bytes: {:#}", total, e),
                    Ok(None) => panic!("soak EOF after {} bytes", total),
                    Err(_) => continue,
                }
            }
            println!("[castflow] soak done: {} bytes total", total);
            return;
        }

        // The dialog's push call.
        let room_label = format!(
            "room {}",
            store.room_id.lock().unwrap().clone().unwrap_or_default()
        );
        crate::dlna::push_to_dlna(&dev.location, &lan_url, Some(room_label))
            .await
            .expect("push_to_dlna must succeed");

        // The renderer itself must confirm PLAYING for a sustained window.
        let soap = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID></u:GetTransportInfo></s:Body></s:Envelope>"#;
        let observe: u64 = std::env::var("DTV_CAST_OBSERVE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(45);
        let mut t = 0u64;
        while t < observe {
            tokio::time::sleep(Duration::from_secs(15)).await;
            t += 15;
            let raw = client.post(&dev.control_url)
                .header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\"")
                .header("Content-Type", "text/xml; charset=utf-8")
                .body(soap)
                .send().await.expect("transport info");
            let text = raw.text().await.unwrap_or_default();
            let state = text.split("<CurrentTransportState>").nth(1)
                .and_then(|s2| s2.split('<').next()).unwrap_or("?");
            println!("[castflow] t={}s {} state={}", t, dev.name, state);
            assert_eq!(state, "PLAYING", "renderer not playing at t={}s", t);
        }
        println!("[castflow] PASS: discovered, served cast url, and renderer stayed PLAYING for {}s", observe);
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
