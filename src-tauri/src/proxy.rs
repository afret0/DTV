use actix_web::{dev::ServerHandle, web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use reqwest::Client;
use crate::StreamUrlStore;
use serde::Deserialize;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::flv_relay;
use tokio::sync::broadcast;
use std::time::Duration;
use tauri::{AppHandle, State};

#[derive(Default)]
pub struct ProxyServerHandle(pub Arc<StdMutex<Option<ServerHandle>>>);

pub type ServerHandleForStore = ServerHandle;

const HUYA_HYSDK_UA: &str =
    "HYSDK(Windows,30000002)_APP(pc_exe&7080000&official)_SDK(trans&2.34.0.5795)";

async fn find_free_port() -> u16 { 34719 }

pub fn build_upstream_request(client: &Client, url: &str) -> reqwest::RequestBuilder {
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
    /// 本场直播的 FLV 文件头 + onMetaData + 音/视频序列头（硬重同步时重放，
    /// 让重连的客户端拿到可解码的起点）。
    init: std::sync::Mutex<(Vec<u8>, bool)>,
    /// 当前会话首个视频序列头 + 最近视频帧间隔（帧率指纹），用于判断中继
    /// 连接是否与当前连接是同一编码变体（30fps/60fps、_4000 档位等）。
    vseq: std::sync::Mutex<Option<bytes::Bytes>>,
    video_delta: std::sync::Mutex<(u32, u32)>,
    /// 连续「饿死重连」次数：反复发生说明续流在这条线上不可用，本连接内关掉
    /// 续流退回普通断线重连，绝不让 pump 卡在重同步循环里（用户看到黑屏）。
    starvation_streak: std::sync::atomic::AtomicU32,
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
        /// FLV absolute timestamp (ms) of this tag.
        timestamp: u32,
        tag_type: u8,
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
            let timestamp = ((self.pending[7] as u32) << 24)
                | ((self.pending[4] as u32) << 16)
                | ((self.pending[5] as u32) << 8)
                | (self.pending[6] as u32);
            let tag: Vec<u8> = self.pending.drain(..total).collect();
            let is_keyframe = tag_type == 9
                && tag.len() > 11
                && (tag[11] >> 4) == 1
                // Exclude the AVC sequence header (frame type 1, codec 7, pkt 0).
                && !(tag.len() > 12 && tag[11] == 0x17 && tag[12] == 0x00);
            out.push(FlvUnit::Tag { bytes: bytes::Bytes::from(tag), is_keyframe, timestamp, tag_type });
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

static PUMP: OnceLock<StdMutex<Option<(std::sync::Weak<StdMutex<String>>, Arc<SharedPump>)>>> =
    OnceLock::new();
static PUMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn pump_slot() -> &'static StdMutex<Option<(std::sync::Weak<StdMutex<String>>, Arc<SharedPump>)>> {
    PUMP.get_or_init(|| StdMutex::new(None))
}

/// Returns the pump bound to this store; a different store (room/platform
/// switch) replaces the previous pump and cancels it.
fn pump_for(store: &StreamUrlStore) -> Arc<SharedPump> {
    // Identity is the shared `url` Arc: stable across Data clones and mutations,
    // unique per stream source. It is compared through a Weak upgrade rather than
    // a raw pointer value -- a dropped store frees that address and the allocator
    // can hand the same address to the next test/room, which would resurrect a
    // stale pump still bound to the dead store (no data ever reaching clients).
    let slot = pump_slot().lock().unwrap();
    if let Some((weak_url, p)) = slot.as_ref() {
        if !p.cancelled.load(Ordering::SeqCst) {
            if let Some(strong) = weak_url.upgrade() {
                if Arc::ptr_eq(&strong, &store.url) {
                    return p.clone();
                }
            }
        }
    }
    drop(slot);
    // 广播环形缓冲：容量按“消息数”算。太小 -> 慢客户端（WiFi 投屏、后台标签页）
    // 频繁掉出窗口被断开；太大 -> 积压时吃内存（每条消息是一个上游 chunk，
    // 斗鱼约 10~70KB，1024 条 ≈ 30s 实时余量 / 最坏 ~50MB）。
    let (tx, _rx) = broadcast::channel::<(u64, u64, bytes::Bytes)>(BROADCAST_RING_CAP);
    let id = PUMP_SEQ.fetch_add(1, Ordering::SeqCst) as u64 + 1;
    let p = Arc::new(SharedPump {
        tx,
        primer: StdMutex::new(Primer::default()),
        header_sent: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        task_running: AtomicBool::new(false),
        init: std::sync::Mutex::new((Vec::new(), false)),
        vseq: std::sync::Mutex::new(None),
        video_delta: std::sync::Mutex::new((0, 0)),
        starvation_streak: std::sync::atomic::AtomicU32::new(0),
        task_spawns: AtomicUsize::new(0),
        session_healthy: AtomicBool::new(false),
        broadcast_started: AtomicBool::new(false),
        id,
    });
    let mut slot = pump_slot().lock().unwrap();
    if let Some((_, old)) = slot.take() {
        crate::tl!("proxy", "[proxy] pump #{} created; cancelling previous pump #{}", id, old.id);
        old.cancelled.store(true, Ordering::SeqCst);
    } else {
        crate::tl!("proxy", "[proxy] pump #{} created", id);
    }
    *slot = Some((Arc::downgrade(&store.url), p.clone()));
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
                            crate::tl!("proxy", "[proxy] dropped stale Douyu refresh for room {}", room);
                            return false;
                        }
                        crate::tl!("proxy", "[proxy] refreshed Douyu URL for room {}", room);
                        *store.url.lock().unwrap() = fresh;
                        note_store_sign_now();
                        true
                    }
                    Err(e) => {
                        crate::tl!("proxy", "[proxy] Douyu refresh failed: {}", e);
                        false
                    }
                }
            } else {
                false
            }
        }
        Some("bilibili") => {
            let room = store.room_id.lock().unwrap().clone();
            let quality = store.quality.lock().unwrap().clone();
            if let Some(room) = room {
                // proxy_ctx=None: pure playurl refresh, no store writes inside.
                match crate::platforms::bilibili::stream_url::fetch_bilibili_stream_core(
                    room.clone(),
                    quality.unwrap_or_else(|| "原画".to_string()),
                    None,
                    None,
                )
                .await
                {
                    Ok(info) => {
                        let fresh = info
                            .upstream_url
                            .clone()
                            .or(info.stream_url.clone())
                            .filter(|u| !u.contains(":34719/"));
                        match fresh {
                            Some(url) => {
                                if stale() {
                                    crate::tl!("proxy", "[proxy] dropped stale Bilibili refresh for room {}", room);
                                    return false;
                                }
                                crate::tl!("proxy", "[proxy] refreshed Bilibili URL for room {}", room);
                                *store.url.lock().unwrap() = url;
                                note_store_sign_now();
                                true
                            }
                            None => {
                                crate::tl!("proxy", "[proxy] Bilibili refresh produced no upstream url");
                                false
                            }
                        }
                    }
                    Err(e) => {
                        crate::tl!("proxy", "[proxy] Bilibili refresh failed: {}", e);
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
                            crate::tl!("proxy", "[proxy] dropped stale Huya refresh for room {}", room);
                            return false;
                        }
                        crate::tl!("proxy", "[proxy] refreshed Huya URL for room {}", room);
                        *store.url.lock().unwrap() = fresh;
                        note_store_sign_now();
                        true
                    }
                    Err(e) => {
                        crate::tl!("proxy", "[proxy] Huya refresh failed: {}", e);
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

/// Commit a freshly written stream source (url/platform/room already set on the
/// store) so the running pump switches over IMMEDIATELY: bump the generation
/// (pump breaks out of the old upstream session, attached clients re-attach to
/// the new one), stamp the sign time and invalidate the primer.
///
/// Every store-URL writer MUST call this. Skipping it (as the bilibili path
/// once did) leaves the pump streaming the PREVIOUS platform until a natural
/// EOF (~5 min for Douyu): switching platforms appeared to "take forever" and,
/// when the switch finally happened mid-session, the FLV header and codec
/// sequence headers were dropped as "duplicates" -> players showed 解码错误
/// until refreshed.
pub fn commit_new_source(store: &StreamUrlStore) {
    store.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    note_store_sign_now();
    reset_pump_header();
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

/// 斗鱼直链固定 300s 到期。登记续流上下文后 pump 会在到期前预取下一条并做
/// tag 级无缝接续；其他平台返回 None，保持原有一次性直通 + 断线重签路径。
fn douyu_renew_context(store: &StreamUrlStore) -> Option<flv_relay::StreamRenewContext> {
    if store.platform.lock().unwrap().as_deref() != Some("douyu") {
        return None;
    }
    let room_id = store.room_id.lock().unwrap().clone()?;
    let quality = store.quality.lock().unwrap().clone()?;
    let line = store.line.lock().unwrap().clone();
    if room_id.is_empty() || quality.is_empty() {
        return None;
    }
    Some(flv_relay::StreamRenewContext { room_id, quality, line })
}

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
        crate::tl!("proxy", "[proxy] pump #{} hit the {} respawn cap; not restarting", p.id, MAX_PUMP_RESPAWNS);
        return;
    }
    if spawns > 0 {
        crate::tl!("proxy", "[proxy] restarting pump #{} (respawn {})", p.id, spawns);
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
        crate::tl!("proxy", "[proxy] pump #{} task started", pump_id);
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
                crate::tl!("proxy", "[proxy] pump refused self-referential URL");
                break;
            }

            // 没人看就先别建上游连接：签名/带宽都省下来，客户端一来立刻起流。
            if CLIENT_COUNT.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            let mut gen_at_connect = store.generation.load(Ordering::SeqCst);
            let connected_at = std::time::Instant::now();
            let mut rejected = false;
            let mut session_bytes: u64 = 0;
            let mut session_chunks: u64 = 0;
            let mut beat_at = std::time::Instant::now();
            let mut last_beat: u64 = 0;
            match build_upstream_request(&client, &url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    attempts = 0;
                    let mut stream = flv_relay::SourceBody::Http(Box::pin(resp.bytes_stream()));
                    p.session_healthy.store(false, Ordering::SeqCst);
                    // init / 序列头指纹 / 帧率指纹按连接重采：重放与变体判定都必须
                    // 基于当前这条连接的配置，拿上一个房间的旧值会误判+黑屏。
                    *p.init.lock().unwrap() = (Vec::new(), false);
                    *p.vseq.lock().unwrap() = None;
                    *p.video_delta.lock().unwrap() = (0, 0);
                    // Is this a brand new source (room/quality switch) or a
                    // reconnect of the same stream after a signature expired?
                    let new_source = !p.header_sent.load(Ordering::SeqCst);
                    // A reconnect must resume on a keyframe boundary with whole
                    // tags, or the decoder shows mosaic until the next GOP. A new
                    // source forwards everything (header + config + keyframe).
                    if new_source {
                        // Fresh stream: the primer rebuilds from this header.
                        p.primer.lock().unwrap().reset(gen_at_connect);
                    } else {
                        // Same stream: keep the primer's header/config/keyframe so
                        // late joiners stay primed across the reconnect gap.
                        p.primer.lock().unwrap().clear_pending();
                    }

                    // 斗鱼直链固定 300s 后 EOF。只有登记了续流上下文才做无缝接续
                    // （虎牙/B站/抖音保持原有一次性直通 + 断线重签路径）。
                    let renew_ctx = douyu_renew_context(&store);
                    let mut renewal = renew_ctx.is_some();
                    if renewal && p.starvation_streak.load(Ordering::SeqCst) >= 3 {
                        crate::tl!(
                            "proxy",
                            "pump #{} seamless renewal DISABLED for this connection (starvation streak={}): plain reconnect only",
                            pump_id, p.starvation_streak.load(Ordering::SeqCst)
                        );
                        renewal = false;
                    }
                    // 接续段标记：只有「斗鱼无缝续流」的新段走 tag 级去重路径。
                    // 普通断线重连必须继续走 framer 直通 + 关键帧对齐路径，否则
                    // 无法按 FLV 解析的上游（如测试用的填充数据）会被整段丢弃。
                    let mut continuation = !new_source && renewal;
                    // 重连（非首段）时，首段直通路径必须等到视频关键帧才开始转发，
                    // 否则从 GOP 中间接进去会花屏；斗鱼续流走 tag 级去重路径，不在此列。
                    let mut waiting_keyframe = !new_source;
                    let mut segment = flv_relay::Segment::new(continuation);
                    let mut framer = FlvFramer::new();
                    let mut demuxer = flv_relay::FlvDemuxer::default();
                    let mut renew_failures: u32 = 0;
                    let mut segment_swaps: u32 = 0;
                    // 已转发到客户端的最新音视频时间戳（接续去重靠它）。
                    let mut last_video_fwd: u32 = 0;
                    let mut last_audio_fwd: u32 = 0;
                    // 后台中继：新连接一建立就由独立任务**持续**把字节送进通道。
                    // 关键：取直链、建连、追平直播沿这些耗时的事全在独立任务里做，pump
                    // 主循环从头到尾没有一次为续流而停顿的 await。之前的「预取一小段缓冲」
                    // 设计行不通 —— 斗鱼新连接的内容比直播沿落后数秒且同样 1x 推进，
                    // 固定缓冲永远凑不出超前的内容；只有持续中继才能在切换瞬间就有活数据。
                    let (stage_tx, mut stage_rx) = tokio::sync::mpsc::unbounded_channel::<
                        Result<flv_relay::StagedSegment, String>,
                    >();
                    let mut staging = false;
                    let mut relay: Option<flv_relay::StagedSegment> = None;
                    // IDR 对齐切换点：中继超前标记后，记下它下一个关键帧的时间戳 K。
                    // 旧流发到 K 之前一帧为止，然后新流从 K（IDR）开始发 —— 解码器
                    // 参考链在切换点重置。中途拼接会破坏 H.264 参考链（两条连接的
                    // frame_num/POC 不同源），播放器几秒后报 5103「解码错误」。
                    let mut pending_cut: Option<u32> = None;
                    // 延迟发布队列：(入队时间, payload, 入队时的视频标记)。见 RELEASE_DELAY。
                    let mut pending_out: Vec<(std::time::Instant, bytes::Bytes, u32)> = Vec::new();
                    // 硬重同步后：丢弃内容直到下一个 IDR 再恢复转发。
                    let mut await_idr = false;
                    let mut last_diag: Option<std::time::Instant> = None;
                    let release_delay = release_delay();
                    // 中继与当前连接编码变体不一致时禁止拼接，只能硬重同步。
                    let mut relay_incompatible = false;
                    // 上一次中继尝试的时间：失败后留 3s 退避，避免每个 chunk 都重打播放接口。
                    let mut last_stage_attempt: Option<tokio::time::Instant> = None;
                    let mut splice_logged = [false, false]; // [video, audio]
                    let mut config_fp = [0u64; 2]; // [video, audio] 序列头指纹
                    // 「有数据进来但什么都没转发出去」持续多久了。真出现说明续流的时间戳
                    // 标记已失效（例如新连接内容长期落后于标记、全被去重丢掉），必须整体
                    // 重同步，绝不能带着冻结的标记无限空转（用户看到的就是永久转圈）。
                    let mut starved_since: Option<std::time::Instant> = None;

                    // 硬重同步：bump generation 让所有客户端干净重连（拿 primer），
                    // 同时重放 init（文件头+配置）并从 IDR 恢复内容。用于两种绝境：
                    //  1) 旧流 EOF 时中继仍落后（CDN 回吐深度 > 余量）：按时间戳去重
                    //     拼接会先饿几秒再从中途接 GOP -> 参考链断裂 = 解码错误；
                    //  2) 有数据但长期零转发（标记失效）。
                    // 重连的客户端从新位置起播（可能比直播沿落后几秒），换来的是
                    // 不花屏、不报解码错误、约 1~2s 的换台感。
                    macro_rules! hard_resync {
                        ($st:expr) => {{
                            pending_out.clear();
                            pending_cut = None;
                            staging = false;
                            relay = None;
                            commit_new_source(&store);
                            gen_at_connect = store.generation.load(Ordering::SeqCst);
                            last_video_fwd = 0;
                            last_audio_fwd = 0;
                            let init = p.init.lock().unwrap().0.clone();
                            let init_len = init.len();
                            if !init.is_empty() {
                                let payload = bytes::Bytes::from(init);
                                let offset_before = offset_total;
                                offset_total = offset_total.wrapping_add(payload.len() as u64);
                                p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                                p.header_sent.store(true, Ordering::SeqCst);
                                p.broadcast_started.store(true, Ordering::SeqCst);
                                let _ = tx.send((gen_at_connect, offset_before, payload));
                            }
                            {
                                let mut st = $st;
                                {
                                    st.consumed.store(true, Ordering::Relaxed);
                                    st.poll();
                                    let drained = st.take_buffered();
                                    demuxer.reset();
                                    demuxer.push(&drained);
                                    demuxer.take_header();
                                    let mut payload = bytes::BytesMut::new();
                                    let mut kept = 0usize;
                                    let mut seen_key = false;
                                    while let Some(tag) = demuxer.next_tag() {
                                        let keep = match tag.tag_type {
                                            flv_relay::TAG_TYPE_VIDEO => {
                                                if !seen_key {
                                                    seen_key = tag.is_keyframe && !tag.is_sequence_header;
                                                    seen_key
                                                } else {
                                                    true
                                                }
                                            }
                                            flv_relay::TAG_TYPE_AUDIO => seen_key,
                                            _ => false,
                                        };
                                        if !keep {
                                            continue;
                                        }
                                        match tag.tag_type {
                                            flv_relay::TAG_TYPE_VIDEO => last_video_fwd = tag.timestamp,
                                            flv_relay::TAG_TYPE_AUDIO => last_audio_fwd = tag.timestamp,
                                            _ => {}
                                        }
                                        payload.extend_from_slice(&tag.raw);
                                        kept += 1;
                                    }
                                    if !payload.is_empty() {
                                        let payload = payload.freeze();
                                        let offset_before = offset_total;
                                        offset_total = offset_total.wrapping_add(payload.len() as u64);
                                        p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                                        let _ = tx.send((gen_at_connect, offset_before, payload));
                                    }
                                    let url = st.url.clone();
                                    let preage = st.relay_started_at.elapsed();
                                    // 会话指纹切换到新变体，后续续流以它为基准比较。
                                    *p.vseq.lock().unwrap() = st.video_seq.clone();
                                    *p.video_delta.lock().unwrap() = (0, 0);
                                    *store.url.lock().unwrap() = url;
                                    note_store_sign_now();
                                    renew_failures = 0;
                                    segment_swaps += 1;
                                    crate::tl!(
                                        "proxy",
                                        "[proxy] pump #{} HARD RESYNC swap #{} at offset {}: replayed {} init bytes + {} tags from relay IDR (clients reconnect with primer)",
                                        pump_id, segment_swaps, offset_total, init_len, kept
                                    );
                                    stream = st.body;
                                    continuation = true;
                                    waiting_keyframe = false;
                                    await_idr = false;
                                    capture_init_from(&p, &drained);
                                    segment = flv_relay::Segment::new_with_preage(true, preage);
                                    demuxer.reset();
                                }
                            }
                        }};
                    }

                    macro_rules! switch_to_relay {
                        ($st:expr, $cut:expr) => {
                            apply_relay_switch(
                                $st,
                                $cut,
                                &mut pending_out,
                                &p,
                                &tx,
                                &store,
                                gen_at_connect,
                                &mut offset_total,
                                &mut last_video_fwd,
                                &mut last_audio_fwd,
                                &mut config_fp,
                                &mut stream,
                                &mut continuation,
                                &mut waiting_keyframe,
                                &mut segment,
                                &mut demuxer,
                                &mut renew_failures,
                                &mut segment_swaps,
                                &note_store_sign_now,
                                pump_id,
                            )
                        };
                    }

                    // 无客户端计时放在段循环之外：续流切换/重同步的 continue 不能重置它，
                    // 否则没人看的时候 pump 会永远烧带宽。
                    let mut idle_since: Option<std::time::Instant> = None;
                    'segments: loop {
                        // 上游静默 / 无客户端 两个看门狗都靠 chunk 循环里判定。
                        #[allow(unused_assignments)]
                        let mut silent = false; // true = 静默超时断开（不是正常 EOF）
                        loop {
                            let chunk = match tokio::time::timeout(
                                UPSTREAM_SILENCE_TIMEOUT,
                                stream.next_chunk(),
                            )
                            .await
                            {
                                Ok(Some(c)) => c,
                                Ok(None) => {
                                    silent = false;
                                    break;
                                }
                                Err(_) => {
                                    silent = true;
                                    break;
                                }
                            };
                            // 延迟发布：到期的旧流包才发给客户端（保证中继先看到 K）。
                            while pending_out.first().map_or(true, |(t, _, _)| t.elapsed() >= release_delay)
                                && !pending_out.is_empty()
                            {
                                let (_, payload, _) = pending_out.remove(0);
                                let offset_before = offset_total;
                                offset_total = offset_total.wrapping_add(payload.len() as u64);
                                p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                                p.header_sent.store(true, Ordering::SeqCst);
                                p.broadcast_started.store(true, Ordering::SeqCst);
                                let _ = tx.send((gen_at_connect, offset_before, payload));
                            }

                            // 没人看了就别再烧上行带宽：过一会儿自行退出，
                            // 下次有客户端请求时 ensure_pump 会重新拉起。
                            if CLIENT_COUNT.load(Ordering::Relaxed) == 0 {
                                match idle_since {
                                    Some(t) if t.elapsed() >= IDLE_PUMP_SHUTDOWN => {
                                        crate::tl!(
                                            "proxy",
                                            "pump #{} idle (0 clients) for {}s -> shutting down, will respawn on next request",
                                            pump_id,
                                            IDLE_PUMP_SHUTDOWN.as_secs()
                                        );
                                        return;
                                    }
                                    Some(_) => {}
                                    None => idle_since = Some(std::time::Instant::now()),
                                }
                            } else {
                                idle_since = None;
                            }
                            if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                                crate::tl!("proxy", "[proxy] stream source changed -> switch upstream");
                                break 'segments;
                            }
                            session_bytes += chunk.len() as u64;
                            session_chunks += 1;
                            last_beat += chunk.len() as u64;
                            if beat_at.elapsed() >= Duration::from_secs(60) {
                                crate::tl!(
                                    "proxy",
                                    "pump #{} heartbeat: {} clients, +{} bytes/60s, total {} bytes/{} chunks, lived {:.0}s, url_age_ms={}, renewals={}",
                                    pump_id,
                                    CLIENT_COUNT.load(Ordering::Relaxed),
                                    last_beat,
                                    session_bytes,
                                    session_chunks,
                                    connected_at.elapsed().as_secs_f32(),
                                    sign_age_ms(),
                                    segment_swaps
                                );
                                beat_at = std::time::Instant::now();
                                last_beat = 0;
                            }
                            if session_bytes > 200_000
                                && !p.session_healthy.swap(true, Ordering::SeqCst)
                            {
                                // Genuinely streaming now: forget earlier respawn
                                // attempts so normal use is unlimited.
                                p.task_spawns.store(0, Ordering::SeqCst);
                                p.starvation_streak.store(0, Ordering::SeqCst);
                            }

                            // 临近 TTL 时启动后台中继（只启动一次；失败后 3s 退避重试）。
                            if renewal
                                && !staging
                                && relay.is_none()
                                && segment.should_prefetch()
                                && last_stage_attempt.map_or(true, |t| t.elapsed() >= Duration::from_secs(3))
                            {
                                last_stage_attempt = Some(tokio::time::Instant::now());
                                if let Some(ctx) = renew_ctx.clone() {
                                    let c = client.clone();
                                    let txc = stage_tx.clone();
                                    let want = flv_relay::url_variant_sig(&store.url.lock().unwrap());
                                    tokio::spawn(async move {
                                        let r = flv_relay::open_staged_relay(&ctx, &c, Some(want)).await;
                                        let _ = txc.send(r);
                                    });
                                    staging = true;
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} opening staged relay in background at t={:.1}s (non-blocking)",
                                        pump_id,
                                        segment.elapsed().as_secs_f64()
                                    );
                                }
                            }

                            // 中继就绪：先拿着，等旧流 EOF 时切换（切换前排空积压）。
                            match stage_rx.try_recv() {
                                Ok(Ok(st)) => {
                                    if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                                        crate::tl!("proxy", "pump #{} relay ready but source changed -> dropping it", pump_id);
                                        break 'segments;
                                    }
                                    staging = false;
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} staged relay ready, {}ms after open (will splice at old-stream EOF)",
                                        pump_id,
                                        st.relay_started_at.elapsed().as_millis()
                                    );
                                    relay = Some(st);
                                }
                                Ok(Err(e)) => {
                                    staging = false;
                                    renew_failures += 1;
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} staged relay failed ({}/{}): {}",
                                        pump_id, renew_failures, flv_relay::MAX_CONSECUTIVE_FAILURES, e
                                    );
                                }
                                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                    staging = false;
                                }
                            }

                            // 续流平台：旁路解析，只为记下时间轴位置供接续对齐。
                            // 首段的转发仍是原始上游字节块，写入粒度不变。
                            if renewal && !continuation {
                                demuxer.push(&chunk);
                                demuxer.take_header();
                                while demuxer.next_tag().is_some() {}
                                if demuxer.is_desynced() {
                                    crate::tl!("proxy", "[proxy] upstream is not parsable FLV; disabling seamless renewal");
                                    renewal = false;
                                    demuxer.reset();
                                }
                            }

                            let mut kept_this_chunk: usize = 0;
                            if continuation {
                                // 接续段：整 tag 裁剪后聚合发出。
                                let mut payload = bytes::BytesMut::with_capacity(chunk.len());
                                let mut kept = 0;
                                let mut dropped_audio = 0;
                                let mut dropped_video = 0;
                                let mut cut_now = false;
                                {
                                    // 必须复用同一个 demuxer：FLV tag 会被 chunk 边界切开，
                                    // 每个 chunk 新建一个解析器会把半截 tag 丢掉（撕裂的
                                    // tag 让解码器错位 —— 就是用户看到的马赛克/花屏）。
                                    demuxer.push(&chunk);
                                    demuxer.take_header(); // 新段的文件头丢掉，播放器已拿到首段那一个
                                    while let Some(tag) = demuxer.next_tag() {
                                        if await_idr {
                                            // 硬重同步后：从下一个 IDR 重新开始，之前的全丢。
                                            if tag.tag_type == flv_relay::TAG_TYPE_VIDEO
                                                && tag.is_keyframe
                                                && !tag.is_sequence_header
                                            {
                                                await_idr = false;
                                                last_video_fwd = 0;
                                                last_audio_fwd = 0;
                                            } else {
                                                continue;
                                            }
                                        }
                                        match tag.tag_type {
                                            flv_relay::TAG_TYPE_AUDIO => {
                                                // 音频帧很小，只保留时间戳严格超前于已发
                                                // 音频的那些；旧的/重复的一律丢掉，避免
                                                // SourceBuffer.appendBuffer 报 QuotaExceeded。
                                                if tag.timestamp <= last_audio_fwd {
                                                    dropped_audio += 1;
                                                    continue;
                                                }
                                                last_audio_fwd = tag.timestamp;
                                            }
                                            flv_relay::TAG_TYPE_VIDEO => {
                                                if !tag.is_sequence_header {
                                                    note_video_delta(&p, tag.timestamp);
                                                }
                                                if pending_cut
                                                    .map_or(false, |k| tag.timestamp >= k.saturating_add(CUT_GATE_MARGIN_MS))
                                                {
                                                    cut_now = true;
                                                    break; // 旧流到此为止，K 由新流提供
                                                }
                                                if tag.timestamp <= last_video_fwd {
                                                    dropped_video += 1;
                                                    continue;
                                                }
                                                last_video_fwd = tag.timestamp;
                                            }
                                            _ => {
                                                // 序列头/脚本 tag：整场只认第一次。但若新连接
                                                // 的 SPS/PPS 与首段不同（CDN 边缘重封装），丢掉
                                                // 它会让解码器拿着旧配置解新帧 -> 解码错误。
                                                if tag.is_sequence_header {
                                                    note_config(pump_id, tag.tag_type, &tag.raw, &mut config_fp);
                                                }
                                                continue;
                                            }
                                        }
                                        payload.extend_from_slice(&tag.raw);
                                        kept += 1;
                                    }
                                }
                                kept_this_chunk = kept;
                                if kept > 0 {
                                    pending_out.push((std::time::Instant::now(), payload.freeze(), last_video_fwd));
                                }
                                if cut_now {
                                    if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                                        break 'segments;
                                    }
                                    if let Some(st) = relay.take() {
                                        let cut = pending_cut.take();
                                        if switch_to_relay!(st, cut) {
                                            continue 'segments;
                                        }
                                    }
                                    break 'segments;
                                }
                                if kept == 0 && (dropped_audio > 0 || dropped_video > 0)
                                    && !std::mem::replace(
                                        &mut splice_logged[if dropped_video > 0 { 1 } else { 0 }],
                                        true,
                                    )
                                {
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} skipped {} audio tags / {} video tags not ahead of dedup markers (v={} a={})",
                                        pump_id, dropped_audio, dropped_video, last_video_fwd, last_audio_fwd
                                    );
                                }
                            } else {
                                // 首段路径：按整 tag 边界转发，绝不广播半个 FLV tag
                                // （半个 tag 会让解码器错位，出现马赛克直到下一个关键帧）。
                                let units = framer.push(&chunk);
                                if units.is_empty() {
                                    continue; // 还差一个完整 tag，继续攒
                                }
                                let mut out = bytes::BytesMut::with_capacity(chunk.len() + 64);
                                let mut cut_now = false;
                                for unit in units {
                                    match unit {
                                        FlvUnit::Header(h) => {
                                            {
                                                let mut g = p.init.lock().unwrap();
                                                if !g.1 {
                                                    g.0.extend_from_slice(&h);
                                                }
                                            }
                                            // 重连（不是全新源）时客户端已经拿到过 FLV 文件头，
                                            // 再发一次会让 xgplayer/MSE 认为流重新开始（表现为
                                            // 「请刷新」）。primer 同理：它已抓住 head，多喂 13
                                            // 字节会被当成 tag 头解析而错位，所以整块丢弃。
                                            if new_source {
                                                out.extend_from_slice(&h);
                                            }
                                        }
                                        FlvUnit::Raw(r) => out.extend_from_slice(&r),
                                        FlvUnit::Tag { bytes: tag, is_keyframe, timestamp, tag_type } => {
                                            if tag_type == flv_relay::TAG_TYPE_VIDEO
                                                && pending_cut.map_or(false, |k| timestamp >= k.saturating_add(CUT_GATE_MARGIN_MS))
                                            {
                                                cut_now = true;
                                                break; // 旧流到此为止，K 由新流提供
                                            }
                                            if waiting_keyframe {
                                                if tag_type == flv_relay::TAG_TYPE_VIDEO && is_keyframe {
                                                    waiting_keyframe = false;
                                                } else {
                                                    continue; // 关键帧之前的内容丢掉，避免花屏
                                                }
                                            }
                                            let is_seq = match tag_type {
                                                flv_relay::TAG_TYPE_VIDEO => tag.len() > 12 && tag[11] == 0x17 && tag[12] == 0x00,
                                                flv_relay::TAG_TYPE_AUDIO => tag.len() > 12 && tag[12] == 0x00,
                                                _ => false,
                                            };
                                            if is_seq {
                                                note_config(pump_id, tag_type, &tag, &mut config_fp);
                                                if tag_type == flv_relay::TAG_TYPE_VIDEO {
                                                    let mut g = p.vseq.lock().unwrap();
                                                    if g.is_none() {
                                                        *g = Some(tag.clone());
                                                    }
                                                }
                                            } else if tag_type == flv_relay::TAG_TYPE_VIDEO {
                                                note_video_delta(&p, timestamp);
                                            }
                                            {
                                                let mut g = p.init.lock().unwrap();
                                                if !g.1 {
                                                    if is_seq || tag_type == flv_relay::TAG_TYPE_SCRIPT {
                                                        g.0.extend_from_slice(&tag);
                                                    } else if tag_type == flv_relay::TAG_TYPE_VIDEO && is_keyframe {
                                                        g.1 = true; // 配置收集完毕
                                                    }
                                                }
                                            }
                                            match tag_type {
                                                flv_relay::TAG_TYPE_AUDIO => {
                                                    last_audio_fwd = last_audio_fwd.max(timestamp);
                                                }
                                                flv_relay::TAG_TYPE_VIDEO => {
                                                    last_video_fwd = last_video_fwd.max(timestamp);
                                                }
                                                _ => {}
                                            }
                                            out.extend_from_slice(&tag);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    kept_this_chunk = 1;
                                    pending_out.push((std::time::Instant::now(), out.freeze(), last_video_fwd));
                                }
                                if cut_now {
                                    if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                                        break 'segments;
                                    }
                                    if let Some(st) = relay.take() {
                                        let cut = pending_cut.take();
                                        if switch_to_relay!(st, cut) {
                                            continue 'segments;
                                        }
                                    }
                                    break 'segments;
                                }
                            }

                            // 安全网：长时间一帧都发不出去 = 时间轴标记失效，硬重同步，
                            // 绝不能像旧版那样带着冻结的标记无限空转（用户会永远卡住）。
                            if kept_this_chunk == 0 {
                                let since = *starved_since.get_or_insert_with(std::time::Instant::now);
                                if since.elapsed() >= STARVE_TIMEOUT {
                                    // 连续这么多 chunk 都没转发出去任何 tag：说明续流的时间戳
                                    // 标记已经失效（例如新连接的内容长期落后于标记，全被去重
                                    // 丢掉），客户端只会一直转圈。必须整体重同步：bump generation
                                    // 让所有客户端断开重连（拿到干净的 primer），同时重连上游。
                                    // 绝不能像以前那样带着冻结的标记无限空转。
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} kept receiving chunks but forwarded nothing for {:.0}s (markers v={} a={}) -> bumping generation and reconnecting (clients re-GET with a fresh primer)",
                                        pump_id, since.elapsed().as_secs_f32(), last_video_fwd, last_audio_fwd
                                    );
                                    p.starvation_streak.fetch_add(1, Ordering::SeqCst);
                                    commit_new_source(&store);
                                    break 'segments;
                                }
                            } else {
                                starved_since = None;
                            }

                            // 中继超前标记后，把切换点对齐到它的下一个 IDR：旧流继续发到
                            // K 前一帧，切换瞬间新流从 K 开始（见 pending_cut 注释）。
                            let mut relay_dead = false;
                            if let Some(st) = relay.as_mut() {
                                st.poll();
                                // 只要中继已解析到任何超前于标记的关键帧就锁定切换点 K：
                                // 该关键帧的完整 tag 此刻已在中继积压里，而旧流必然更晚
                                // 才到达 K —— 于是「旧流发到 K 前一帧、新流从 K(IDR) 接管」
                                // 这个对齐切换一定可以完成，不依赖两条连接谁更靠前的竞态。
                                if last_diag.map_or(true, |t| t.elapsed() >= Duration::from_secs(5)) {
                                    last_diag = Some(std::time::Instant::now());
                                    // 诊断：中继与客户端发布位置的相对关系（每 5s 一次）。
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} relay diag: relay_v={:?} marker={} keyframes={:?} pending_cut={:?}",
                                        pump_id, st.video_ts(), last_video_fwd,
                                        st.keyframes_dbg(), pending_cut
                                    );
                                }
                                if pending_cut.is_none() && !relay_incompatible {
                                    if relay_compatible(&p, st) == Some(false) {
                                        relay_incompatible = true;
                                        crate::tl!(
                                            "proxy",
                                            "pump #{} relay is a DIFFERENT encoding variant (seq/fps mismatch: cur_delta={}ms relay_delta={}ms) -> splice disabled, will hard resync at EOF",
                                            pump_id, p.video_delta.lock().unwrap().1, st.last_video_delta
                                        );
                                    }
                                }
                                if pending_cut.is_none() && !relay_incompatible {
                                    // K 取中继「最近解析到的关键帧」：它必然已在中继积压里。
                                    // 切换在旧流解析到 K+余量 时执行：此刻客户端（延迟发布）
                                    // 只看到 K 之前，K 本身要么由旧流队列送达、要么由中继
                                    // 送达（cut_floor 兜底），参考链始终完整。
                                    pending_cut = st.last_keyframe()
                                        .filter(|k| k.saturating_add(CUT_GATE_MARGIN_MS) > last_video_fwd.saturating_sub(30_000));
                                    if let Some(k) = pending_cut {
                                        crate::tl!(
                                            "proxy",
                                            "pump #{} cut scheduled at IDR K={} (gate at K+{}ms; relay v={:?}, marker={}, old stream has {:.1}s left)",
                                            pump_id, k, CUT_GATE_MARGIN_MS, st.video_ts(), last_video_fwd,
                                            flv_relay::STREAM_TTL.as_secs_f32() - segment.elapsed().as_secs_f32()
                                        );
                                    }
                                }
                                // 中继连接已结束且没有待完成的切换点：这条废连接留着只会
                                // 挡住下一次预取，丢掉它让 pump 重新取流。
                                relay_dead = st.is_done() && pending_cut.is_none();
                            }
                            if relay_dead {
                                crate::tl!(
                                    "proxy",
                                    "pump #{} staged relay ended before it got ahead (v={:?} marker={}) -> discarding it and re-prefetching",
                                    pump_id,
                                    relay.as_ref().and_then(|s| s.video_ts()),
                                    last_video_fwd
                                );
                                relay = None;
                                staging = false;
                                pending_cut = None;
                            }
                        }

                        // ---- 上游段结束（正常 EOF 或静默超时）----
                        // 切换/重连逻辑会把队列发完或清空；这里只处理直接退出的情况。
                        if silent {
                            crate::tl!(
                                "proxy",
                                "[proxy] pump #{} upstream silent for {}s, {} clients, lived {:?} -> dropping the connection and renewing",
                                pump_id,
                                UPSTREAM_SILENCE_TIMEOUT.as_secs(),
                                CLIENT_COUNT.load(Ordering::Relaxed),
                                connected_at.elapsed()
                            );
                        }
                        if !renewal || renew_ctx.is_none() {
                            for (_, payload, _) in pending_out.drain(..) {
                                let offset_before = offset_total;
                                offset_total = offset_total.wrapping_add(payload.len() as u64);
                                p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                                let _ = tx.send((gen_at_connect, offset_before, payload));
                            }
                            break;
                        }
                        if store.generation.load(Ordering::SeqCst) != gen_at_connect {
                            pending_out.clear();
                            break;
                        }

                        // 走到 EOF 说明旧流没赶上切换点 K（GOP 比剩余寿命还长的罕见情况）：
                        // 退回按时间戳去重的兜底拼接（可能花屏几秒，好过断流）。
                        if pending_cut.take().is_some() {
                            crate::tl!(
                                "proxy",
                                "pump #{} old stream EOF before reaching the scheduled cut -> falling back to dedup splice",
                                pump_id
                            );
                        }
                        // 有后台中继就直接用；在跑就等它交回；都没有就同步开一条。
                        let mut seg = relay.take();
                        if seg.is_none() && staging {
                            staging = false;
                            seg = match tokio::time::timeout(
                                flv_relay::STAGE_TIMEOUT + Duration::from_secs(5),
                                stage_rx.recv(),
                            )
                            .await
                            {
                                Ok(Some(Ok(st))) => Some(st),
                                Ok(Some(Err(e))) => {
                                    renew_failures += 1;
                                    crate::tl!(
                                        "proxy",
                                        "pump #{} staged relay failed at EOF ({}/{}): {}",
                                        pump_id, renew_failures, flv_relay::MAX_CONSECUTIVE_FAILURES, e
                                    );
                                    None
                                }
                                Ok(None) => None,
                                Err(_) => {
                                    crate::tl!("proxy", "pump #{} staged relay did not answer in time; opening synchronously", pump_id);
                                    None
                                }
                            };
                        }
                        if seg.is_none() {
                            let want = flv_relay::url_variant_sig(&store.url.lock().unwrap());
                            match flv_relay::open_staged_relay(renew_ctx.as_ref().unwrap(), &client, Some(want)).await {
                                Ok(st) => seg = Some(st),
                                Err(e) => {
                                    renew_failures += 1;
                                    crate::tl!(
                                        "proxy",
                                        "[proxy] pump #{} renewal failed {}/{}: {}",
                                        pump_id, renew_failures, flv_relay::MAX_CONSECUTIVE_FAILURES, e
                                    );
                                }
                            }
                        }
                        match seg {
                            Some(mut st) => {
                                st.poll();
                                let cut = if relay_incompatible {
                                    None
                                } else {
                                    pending_cut
                                        .take()
                                        .or_else(|| st.first_keyframe_after(last_video_fwd))
                                };
                                if !relay_incompatible
                                    && (cut.is_some() || st.video_ts().map_or(false, |v| v > last_video_fwd))
                                {
                                    if switch_to_relay!(st, cut) {
                                        continue 'segments;
                                    }
                                    break;
                                }
                                // 中继仍落后于标记：去重拼接会先饿几秒再从中途接 GOP，
                                // 参考链必断（5103 解码错误）。改为硬重同步：客户端带
                                // primer 重连，从新连接的 IDR 干净起播。
                                crate::tl!(
                                    "proxy",
                                    "pump #{} relay still behind at EOF (relay v={:?} marker={}) -> hard resync instead of dedup splice",
                                    pump_id, st.video_ts(), last_video_fwd
                                );
                                hard_resync!(st);
                                continue 'segments;
                            }
                            None => {
                                for (_, payload, _) in pending_out.drain(..) {
                                    let offset_before = offset_total;
                                    offset_total = offset_total.wrapping_add(payload.len() as u64);
                                    p.primer.lock().unwrap().feed(&payload, gen_at_connect, offset_total);
                                    let _ = tx.send((gen_at_connect, offset_before, payload));
                                }
                                if renew_failures >= flv_relay::MAX_CONSECUTIVE_FAILURES {
                                    renewal = false;
                                    break;
                                }
                                tokio::time::sleep(flv_relay::RETRY_DELAY).await;
                                break;
                            }
                        }
                    }
                    crate::tl!("proxy", 
                        "[proxy] pump #{} upstream EOF after {} chunks/{} bytes, {} clients, lived {:?}, renewals={} -> reconnect",
                        pump_id,
                        session_chunks,
                        session_bytes,
                        CLIENT_COUNT.load(Ordering::Relaxed),
                        connected_at.elapsed(),
                        segment_swaps
                    );
                }
                Ok(resp) => {
                    crate::tl!("proxy", "[proxy] pump upstream status {} -> retry", resp.status());
                    rejected = true;
                }
                Err(e) => crate::tl!("proxy", "[proxy] pump connect error: {} -> retry", e),
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
            let lived = connected_at.elapsed();
            // Douyu signatures stay valid ~300s from ISSUE time and a cut
            // session does not burn the URL. If the store already holds a
            // recently signed URL (the frontend just loaded the room, or the
            // cut happened well before the signature expired), reconnect with
            // it directly: no extra signing
            // latency for viewers, no extra throttle risk. Re-sign only when the
            // stored URL is old or the CDN actively rejected it.
            let age = sign_age_ms();
            let natural_expiry = lived >= Duration::from_secs(240);
            if rejected || natural_expiry || age >= 240_000 {
                crate::tl!("proxy", 
                    "[proxy] pump #{} re-signing upstream (rejected={} lived={:?} url_age_ms={})",
                    pump_id, rejected, lived, age
                );
                refresh_store_url(&store, gen_at_connect, &p.cancelled).await;
            } else {
                crate::tl!("proxy", 
                    "[proxy] pump #{} reconnecting with fresh stored URL (age={}ms lived={:?})",
                    pump_id, age, lived
                );
            }
            // Healthy long session ending (normal expiry): reconnect fast so the
            // stall stays short. Repeated quick failures: back off instead of
            // hammering the CDN and the signing API.
            let backoff_ms = if lived >= Duration::from_secs(60) {
                300
            } else {
                (1200u64 * attempts as u64).min(10_000)
            };
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
    });
}

/// 上游静默看门狗：连接还开着但这么久没有任何字节 -> 判定为 CDN 卡死，主动断开重连。
/// 没有它，客户端只会一直转圈（斗鱼偶发的「看着看着就不动了」）。
pub const UPSTREAM_SILENCE_TIMEOUT: Duration = Duration::from_secs(20);
/// 没有任何客户端时，pump 继续拉流这么久后自行退出（下次请求会重新拉起），
/// 避免后台一直烧带宽。
pub const IDLE_PUMP_SHUTDOWN: Duration = Duration::from_secs(90);

pub const BROADCAST_RING_CAP: usize = 1024;

/// 收到 chunk 但连续这么久一帧都没转发出去 -> 判定续流时间轴标记失效，整体重同步。
/// （远大于一个 GOP，正常的关键帧等待不会误触发。）
pub const STARVE_TIMEOUT: Duration = Duration::from_secs(15);

/// 旧流数据包在队列里延迟这么久才发给客户端：给切换留出操作空间，客户端
/// 缓冲因此多 2s（斗鱼场景缓冲本就 4s+，无感）。
pub const RELEASE_DELAY: Duration = Duration::from_millis(2000);

/// 切换门限余量：旧流解析到 K+余量 才执行切换。实测旧流解析比中继解析快
/// ~150ms；留 600ms 保证切换排空时 K 已在中继积压里（或紧随其后到达，
/// 由 cut_floor 兜住），客户端一定能拿到 K 这个 IDR。
pub const CUT_GATE_MARGIN_MS: u32 = 600;

/// 单测（mock 上游）不需要竞态保护，可用 DTV_RELEASE_DELAY_MS=0 关掉延迟发布。
fn release_delay() -> Duration {
    std::env::var("DTV_RELEASE_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(RELEASE_DELAY)
}

pub static CLIENT_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Epoch-ms when the store URL was last signed (by the frontend or the pump's
/// re-sign). Douyu signatures live ~300s; knowing
/// the age lets the pump reuse a still-fresh URL on EOF instead of issuing yet
/// another sign request at the worst moment (extra latency for viewers and
/// extra throttle risk from hammering betard/h5enc/playUrl).
static LAST_SIGN_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_store_sign_now() {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    LAST_SIGN_MS.store(ms, Ordering::SeqCst);
}

fn sign_age_ms() -> u64 {
    let last = LAST_SIGN_MS.load(Ordering::SeqCst);
    if last == 0 {
        return u64::MAX;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    now.saturating_sub(last)
}

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
    /// 下一个字节的广播偏移（None = 还没收到第一条，此时从哪儿起都算对齐）。
    /// 广播消息必须首尾相接：中间少了字节说明这个客户端被环形缓冲甩掉了
    /// （消息被覆盖）。继续发下去客户端拿到的是撕裂的 FLV tag —— 表现就是
    /// 花屏/马赛克、解码错误、播放器提示「请刷新」。所以一旦发现空洞就立刻
    /// 结束响应，让客户端重连并从 primer 对齐续读。
    next_offset: Option<u64>,
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
                        self.next_offset = Some(end);
                        return std::task::Poll::Ready(Some(Ok(bytes.slice(skip..))));
                    }
                    if let Some(expect) = self.next_offset {
                        if offset != expect {
                            crate::tl!(
                                "proxy",
                                "[proxy] client stream hole: expected offset {} but got {} ({} bytes missing) -> ending response so it reconnects with a fresh primer",
                                expect,
                                offset,
                                offset.saturating_sub(expect)
                            );
                            return std::task::Poll::Ready(None);
                        }
                    }
                    self.next_offset = Some(end);
                    return std::task::Poll::Ready(Some(Ok(bytes)));
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    // 客户端读得比 pump 广播得慢，环形缓冲把它落后的消息覆盖了。
                    // 绝不能静默跳过：被跳过的字节会让客户端的 FLV 流错位
                    // （撕裂的 tag -> 花屏 / 解码错误 / 播放器提示「请刷新」）。
                    // 正确做法是结束这个响应，客户端自动重连后会拿到 primer
                    // （文件头 + 配置 + 最近关键帧）并从对齐的位置续读。
                    crate::tl!(
                        "proxy",
                        "[proxy] client lost the broadcast window ({}) -> ending response so it reconnects with a fresh primer",
                        e
                    );
                    return std::task::Poll::Ready(None);
                }
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
        crate::tl!("proxy", 
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
    crate::tl!("proxy", "[Rust/proxy.rs handler] Incoming FLV proxy request -> {}", url);

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
                    crate::tl!("proxy", 
                        "[proxy] client #{} late join (gen={}, pump #{}): primer {} bytes @ offset {}",
                        n, current_gen, p.id, b.len(), off
                    );
                    (Some(b), off)
                }
                None => {
                    crate::tl!("proxy", "[proxy] client #{} joined (gen={}, pump #{}): stream from start", n, current_gen, p.id);
                    (None, 0)
                }
            }
            } else {
                crate::tl!("proxy", "[proxy] client #{} joined (gen={}, pump #{}): primer gen mismatch", n, current_gen, p.id);
                (None, 0)
            }
        }
    };
    // 只有拿到了 primer 的客户端才从 primer_offset 开始要求连续；
    // 全新加入的客户端从当前直播位置起读，第一条消息不算空洞。
    let resume_offset = if primer.is_some() { Some(primer_offset) } else { None };
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
            next_offset: resume_offset,
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

/// 从一段 tag 对齐的字节（新连接积压的开头，含 FLV 头）里提取「FLV 头 +
/// onMetaData + 音/视频序列头」作为本场 init，并记下视频序列头指纹。硬重同步
/// 重放 init 时必须用**当前这条连接**的配置，拿旧房间的配置重放会直接黑屏。
fn capture_init_from(p: &SharedPump, buf: &[u8]) {
    let mut demux = flv_relay::FlvDemuxer::default();
    demux.push(buf);
    let mut out = Vec::new();
    if let Some(h) = demux.take_header() {
        out.extend_from_slice(&h);
    }
    let mut vseq: Option<bytes::Bytes> = None;
    while let Some(tag) = demux.next_tag() {
        let is_idr = tag.tag_type == flv_relay::TAG_TYPE_VIDEO
            && tag.is_keyframe
            && !tag.is_sequence_header;
        if is_idr {
            break;
        }
        if tag.tag_type == flv_relay::TAG_TYPE_SCRIPT || tag.is_sequence_header {
            if tag.tag_type == flv_relay::TAG_TYPE_VIDEO && vseq.is_none() {
                vseq = Some(tag.raw.clone());
            }
            out.extend_from_slice(&tag.raw);
        }
    }
    if !out.is_empty() {
        *p.init.lock().unwrap() = (out, true);
    }
    if let Some(v) = vseq {
        *p.vseq.lock().unwrap() = Some(v);
    }
}

/// 记录相邻视频帧的时间戳间隔（帧率指纹）。
fn note_video_delta(p: &SharedPump, timestamp: u32) {
    let mut g = p.video_delta.lock().unwrap();
    let d = timestamp.saturating_sub(g.0);
    if g.0 != 0 && d > 0 && d <= 500 {
        g.1 = d;
    }
    g.0 = timestamp;
}

/// 两条连接是否是同一编码变体（可安全拼接参考链）：
/// 序列头字节、帧率（相邻帧间隔）、URL 档位后缀（如 `_4000`）都必须一致。
/// 斗鱼同一房间的不同取流可能落在不同转码变体上（30fps/60fps），它们的
/// H.264 参考链互不兼容，拼接必出解码错误。
/// 返回 None 表示证据还不够（中继还没解析出帧率指纹），调用方下一轮再判断。
fn relay_compatible(
    p: &SharedPump,
    st: &flv_relay::StagedSegment,
) -> Option<bool> {
    let cur_vseq = p.vseq.lock().unwrap().clone();
    if let (Some(a), Some(b)) = (&cur_vseq, &st.video_seq) {
        if a != b {
            return Some(false);
        }
    }
    let cur_delta = p.video_delta.lock().unwrap().1;
    match (cur_delta, st.last_video_delta) {
        (0, _) | (_, 0) => None, // 帧率指纹还没采到，稍后再判
        (a, b) if a.abs_diff(b) > 8 => Some(false),
        _ => Some(true),
    }
}

/// 记录音/视频序列头（SPS/PPS、AudioSpecificConfig）的指纹。
/// 续流路径会丢弃重复序列头（整场只认第一次），前提是每条连接的配置完全相同；
/// 一旦某条新连接的配置变了还继续丢，解码器就会拿旧配置解新帧 -> 5103 解码错误。
/// 这里只做检测 + 告警，为离线定位提供证据。
fn note_config(pump_id: u64, tag_type: u8, raw: &[u8], fp: &mut [u64; 2]) {
    let idx = match tag_type {
        flv_relay::TAG_TYPE_VIDEO => 0,
        flv_relay::TAG_TYPE_AUDIO => 1,
        _ => return,
    };
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in raw {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let prev = fp[idx];
    fp[idx] = h;
    if prev != 0 && prev != h {
        crate::tl!(
            "proxy",
            "pump #{} codec config CHANGED for {} track (fp {:016x} -> {:016x}, {} bytes) - dropped duplicates would decode with the stale config!",
            pump_id,
            if idx == 0 { "video" } else { "audio" },
            prev,
            h,
            raw.len()
        );
    }
}

/// 把后台中继的新连接接上：先排空通道里已积压的字节（按音/视频轨时间戳去重），
/// 广播差量，然后把 stream 换成中继通道继续读。
///
/// 返回 true 表示已切换（调用方应结束当前段循环）。
#[allow(clippy::too_many_arguments)]
fn apply_relay_switch<F: Fn()>(
    st: flv_relay::StagedSegment,
    cut: Option<u32>,
    pending_out: &mut Vec<(std::time::Instant, bytes::Bytes, u32)>,
    p: &Arc<SharedPump>,
    tx: &broadcast::Sender<(u64, u64, bytes::Bytes)>,
    store: &crate::StreamUrlStore,
    gen_at_connect: u64,
    offset_total: &mut u64,
    last_video_fwd: &mut u32,
    last_audio_fwd: &mut u32,
    config_fp: &mut [u64; 2],
    stream: &mut flv_relay::SourceBody,
    continuation: &mut bool,
    waiting_keyframe: &mut bool,
    segment: &mut flv_relay::Segment,
    demuxer: &mut flv_relay::FlvDemuxer,
    renew_failures: &mut u32,
    segment_swaps: &mut u32,
    note_sign: &F,
    pump_id: u64,
) -> bool {
    let _ = cut; // 仅用于日志：实际去重靠已转发标记
    let mut st = st;
    // 标记「已接手」：中继任务的上限从此失效，否则它会在十几秒后把这条
    // 正在直播的连接掐断（表现为反复重签 + 画面每 20s 断一次）。
    st.consumed.store(true, Ordering::Relaxed);
    st.poll();

    // 延迟发布队列里还有旧流的包（都早于切换点 K）：先按序发完，客户端字节流才连续。
    for (_, payload, _) in pending_out.drain(..) {
        let offset_before = *offset_total;
        *offset_total = offset_total.wrapping_add(payload.len() as u64);
        p.primer.lock().unwrap().feed(&payload, gen_at_connect, *offset_total);
        p.header_sent.store(true, Ordering::SeqCst);
        p.broadcast_started.store(true, Ordering::SeqCst);
        let _ = tx.send((gen_at_connect, offset_before, payload));
    }
    let drained = st.take_buffered();
    // 切换后这条中继连接就是本流：init/指纹换成它的配置。
    capture_init_from(p, &drained);
    let drained_head = {
        let n = drained.len().min(6);
        drained[..n].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
    };
    let flv_relay::StagedSegment { url, body, relay_started_at, .. } = st;

    // 排空积压：按轨去重后剩下的正好接在已转发位置之后。
    let mut payload = bytes::BytesMut::with_capacity(drained.len());
    let mut dropped = 0usize;
    let mut kept = 0usize;
    let mut first_kept_video: Option<(u32, bool)> = None; // (ts, is_keyframe)
    // 复用 pump 的 demuxer：排空得到的字节可能在 tag 中间断开，剩下的半截要留给
    // 切换后的 chunk 继续解析，否则就是撕裂 tag（马赛克/解码错误）。
    demuxer.reset();
    demuxer.push(&drained);
    demuxer.take_header();
    while let Some(tag) = demuxer.next_tag() {
        let duplicate = match tag.tag_type {
            flv_relay::TAG_TYPE_AUDIO => {
                if tag.timestamp <= *last_audio_fwd {
                    dropped += 1;
                    true
                } else {
                    *last_audio_fwd = tag.timestamp;
                    false
                }
            }
            flv_relay::TAG_TYPE_VIDEO => {
                // 一律按「已转发标记」去重：切换点 K 这个 IDR 已经由旧流的延迟
                // 发布队列送达客户端，中继里 K 及其之前的 tag 都是重复内容。
                let dup = tag.timestamp <= *last_video_fwd;
                if dup {
                    dropped += 1;
                    true
                } else {
                    if first_kept_video.is_none() {
                        first_kept_video = Some((tag.timestamp, tag.is_keyframe));
                    }
                    *last_video_fwd = tag.timestamp;
                    false
                }
            }
            _ => {
                if tag.is_sequence_header {
                    note_config(pump_id, tag.tag_type, &tag.raw, config_fp);
                }
                true // 脚本/序列头：整场只认第一次
            }
        };
        if duplicate {
            continue;
        }
        payload.extend_from_slice(&tag.raw);
        kept += 1;
    }

    let offset_before = *offset_total;
    if !payload.is_empty() {
        let payload = payload.freeze();
        *offset_total = offset_total.wrapping_add(payload.len() as u64);
        // primer 喂进去的必须和广播出去的严格一致，迟到客户端的 resume 偏移才对得上。
        p.primer.lock().unwrap().feed(&payload, gen_at_connect, *offset_total);
        p.header_sent.store(true, Ordering::SeqCst);
        p.broadcast_started.store(true, Ordering::SeqCst);
        let _ = tx.send((gen_at_connect, offset_before, payload));
    }

    *store.url.lock().unwrap() = url;
    note_sign();
    *renew_failures = 0;
    *segment_swaps += 1;

    // 新连接已经在中继里活了 `preage`，下一次预取要按它的真实年龄提前触发。
    let preage = relay_started_at.elapsed();
    *stream = body;
    *continuation = true;
    *waiting_keyframe = false;
    *segment = flv_relay::Segment::new_with_preage(true, preage);
    crate::tl!(
        "proxy",
        "[proxy] pump #{} SEAMLESS relay swap #{} at offset {}: cut={:?} relay age {}ms, drained head={} ({} bytes) -> applied {} tags (first kept video ts={:?} keyframe={:?}), dropped {} dup tags, markers v={} a={}, {} clients",
        pump_id,
        *segment_swaps,
        offset_before,
        cut,
        preage.as_millis(),
        drained_head,
        drained.len(),
        kept,
        first_kept_video.map(|(t, _)| t),
        first_kept_video.map(|(_, k)| k),
        dropped,
        last_video_fwd,
        last_audio_fwd,
        CLIENT_COUNT.load(Ordering::Relaxed)
    );
    true
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
            crate::tl!("proxy", "[Rust/proxy.rs] Proxy server run error: {}", e);
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
    crate::tl!("proxy", "stop_proxy called (had_server={})", handle.is_some());
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
            crate::tl!("proxy", "[Rust/proxy.rs] static proxy error: {}", e);
        }
    });

    Ok(format!("http://127.0.0.1:{}", port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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

    /// Bilibili-style source swap: the platform code writes the store directly
    /// (no set_stream_url_cmd) and must call commit_new_source; without the
    /// generation bump the pump would keep streaming the PREVIOUS platform until
    /// its natural EOF (~5 min) and then drop the new FLV header as a
    /// "duplicate" -> players showed 解码错误. The mock CDNs never EOF, so only
    /// an immediate, correct switch can pass.
    #[actix_web::test]
    async fn direct_store_write_with_commit_switches_pump_immediately() {
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
        let cdn_a = mock_cdn_tagged(0xA1).await;
        let cdn_b = mock_cdn_tagged(0xB2).await;
        let port = 39936;
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_a);
        *store.platform.lock().unwrap() = Some("douyu".into());
        let gen_before = store.generation.load(Ordering::SeqCst);

        let srv = build_test_proxy(store.clone(), port);
        actix_web::rt::spawn(srv);
        tokio::time::sleep(Duration::from_millis(400)).await;

        let client = Client::builder().no_proxy().build().unwrap();
        // Read from source A.
        let ra = client.get(format!("http://127.0.0.1:{}/live.flv", port)).send().await.unwrap();
        let mut sa = ra.bytes_stream();
        let mut got_a = Vec::new();
        let dl = tokio::time::Instant::now() + Duration::from_secs(6);
        while got_a.len() < 40_000 && tokio::time::Instant::now() < dl {
            match tokio::time::timeout(Duration::from_millis(600), sa.next()).await {
                Ok(Some(Ok(b))) => got_a.extend_from_slice(&b),
                _ => break,
            }
        }
        assert!(got_a.contains(&0xA1), "source A payload missing");
        assert!(!got_a.contains(&0xB2), "source B leaked into A");

        // Bilibili path: direct store write + commit_new_source.
        *store.url.lock().unwrap() = format!("http://127.0.0.1:{}/live.flv", cdn_b);
        *store.platform.lock().unwrap() = Some("bilibili".into());
        commit_new_source(&store);
        assert_eq!(
            store.generation.load(Ordering::SeqCst),
            gen_before + 1,
            "commit_new_source must bump the generation"
        );

        // A brand-new client must land on B (with a proper FLV start) quickly.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut got_b = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let rb = client.get(format!("http://127.0.0.1:{}/live.flv", port)).send().await.unwrap();
            let mut sb = rb.bytes_stream();
            got_b.clear();
            let dl2 = tokio::time::Instant::now() + Duration::from_secs(4);
            while got_b.len() < 40_000 && tokio::time::Instant::now() < dl2 {
                match tokio::time::timeout(Duration::from_millis(500), sb.next()).await {
                    Ok(Some(Ok(b))) => got_b.extend_from_slice(&b),
                    _ => break,
                }
            }
            if got_b.contains(&0xB2) { break; }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(got_b.starts_with(b"FLV"), "source B client must start with an FLV header");
        assert!(got_b.contains(&0xB2), "pump did not switch to the new source within 10s");
        assert!(!got_b.contains(&0xA1), "old source payload leaked after switch");
        println!("[switch-direct] A->B via commit_new_source ok ({} bytes)", got_b.len());
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "0");
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
    use futures_util::StreamExt;
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
        std::env::set_var("DTV_RELEASE_DELAY_MS", "2000");
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
        // Dedicated test port: 34719 belongs to the real app instance, which may
        // be running while this test executes.
        const LONGRUN_PORT: u16 = 39935;
        super::tests::spawn_test_server(store.clone(), LONGRUN_PORT).await;
        let local_url = format!("http://127.0.0.1:{}/live.flv", LONGRUN_PORT);
        println!("[longrun] proxy ready at {}", local_url);

        let client = Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let resp = client.get(&local_url).send().await.expect("connect");
        assert_eq!(resp.status(), 200, "proxy status");
        let stream = resp.bytes_stream();

        // 客户端可能因硬重同步（generation bump）被断开重连 —— 和真实播放器一样
        // 重新 GET 即可（会拿到 primer）。每个「会话」单独做结构/时间戳分析。
        let mut sessions: Vec<Vec<u8>> = vec![Vec::new()];
        let mut reconnects = 0usize;
        let start = std::time::Instant::now();
        let deadline = start + Duration::from_secs(duration);
        let mut last_report = start;
        let mut last_total: usize = 0;
        let mut total_bytes = 0usize;
        let mut stream = stream;
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                Ok(Some(Ok(b))) => {
                    total_bytes += b.len();
                    sessions.last_mut().unwrap().extend_from_slice(&b);
                }
                Ok(Some(Err(e))) => panic!("stream error at {}s after {} bytes: {:#}",
                    start.elapsed().as_secs(), total_bytes, e),
                Ok(None) => {
                    reconnects += 1;
                    assert!(reconnects <= 5, "too many reconnects ({}) in {}s", reconnects, duration);
                    println!("[longrun] client response ended at {}s (reconnect #{}) - re-GET like a real player",
                        start.elapsed().as_secs(), reconnects);
                    sessions.push(Vec::new());
                    let resp = client.get(&local_url).send().await.expect("reconnect");
                    assert_eq!(resp.status(), 200, "reconnect status");
                    stream = resp.bytes_stream();
                }
                Err(_) => continue,
            }
            if last_report.elapsed() >= Duration::from_secs(60) {
                let delta = total_bytes - last_total;
                println!("[longrun] t={}s total={} bytes (+{} in last 60s)",
                    start.elapsed().as_secs(), total_bytes, delta);
                last_report = std::time::Instant::now();
                last_total = total_bytes;
            }
        }
        println!("[longrun] finished, total {} bytes over {}s in {} session(s)",
            total_bytes, start.elapsed().as_secs(), sessions.len());

        // 逐会话分析：结构完整（无撕裂 tag）、关键帧序列连续、时间戳不回退、无大空洞。
        fn analyze_session(buf: &[u8], label: &str) {
            assert!(buf.starts_with(b"FLV"), "{}: must start with an FLV header", label);
            let mut pos = 13usize;
            let mut tag_count = 0usize;
            while pos + 11 <= buf.len() {
                let size = ((buf[pos + 1] as usize) << 16)
                    | ((buf[pos + 2] as usize) << 8)
                    | (buf[pos + 3] as usize);
                if size > MAX_FLV_TAG {
                    panic!("{}: FLV structure broke at byte {} (torn tag leaked)", label, pos);
                }
                if pos + 11 + size + 4 > buf.len() {
                    break; // 收尾的在途 tag
                }
                let tail = &buf[pos + 11 + size..pos + 11 + size + 4];
                let tail_size = ((tail[0] as usize) << 24) | ((tail[1] as usize) << 16)
                    | ((tail[2] as usize) << 8) | (tail[3] as usize);
                if tag_count > 0 && tail_size != 11 + size {
                    panic!("{}: PreviousTagSize mismatch at byte {} (torn tag leaked)", label, pos);
                }
                tag_count += 1;
                pos += 11 + size + 4;
            }
            let mut walk = 13usize;
            let mut kf_seq: Vec<u32> = Vec::new();
            let mut last_video_ts: Option<u32> = None;
            let mut regressions = 0usize;
            let mut max_video_gap = 0u32;
            let mut max_gap_at: Option<(u32, u32)> = None;
            while walk + 11 <= buf.len() {
                let tag_type = buf[walk] & 0x1f;
                let size = ((buf[walk + 1] as usize) << 16)
                    | ((buf[walk + 2] as usize) << 8)
                    | (buf[walk + 3] as usize);
                if walk + 11 + size + 4 > buf.len() {
                    break;
                }
                let ts = ((buf[walk + 7] as u32) << 24)
                    | ((buf[walk + 4] as u32) << 16)
                    | ((buf[walk + 5] as u32) << 8)
                    | (buf[walk + 6] as u32);
                if tag_type == 9 {
                    let is_seq = size >= 2 && buf[walk + 11] == 0x17 && buf[walk + 12] == 0x00;
                    let is_key = size >= 1 && (buf[walk + 11] >> 4) == 1 && !is_seq;
                    if is_key {
                        kf_seq.push(ts);
                    }
                    if !is_seq {
                        if let Some(prev) = last_video_ts {
                            if ts < prev {
                                regressions += 1;
                            } else if ts.saturating_sub(prev) > max_video_gap {
                                max_video_gap = ts - prev;
                                max_gap_at = Some((prev, ts));
                            }
                        }
                        last_video_ts = Some(ts.max(last_video_ts.unwrap_or(0)));
                    }
                }
                walk += 11 + size + 4;
            }
            for w in kf_seq.windows(2) {
                let d = w[1].saturating_sub(w[0]);
                assert!(
                    d > 0 && d <= 10_000,
                    "{}: keyframe sequence broken at {} -> {} (delta {}ms): a dropped/duplicated IDR breaks the decoder reference chain",
                    label, w[0], w[1], d
                );
            }
            assert!(
                max_video_gap < 5_000,
                "{}: renewal left a {}ms video hole at {:?} -> the player would stall/spin",
                label, max_video_gap, max_gap_at
            );
            assert_eq!(regressions, 0, "{}: video timestamps went backwards", label);
            assert!(kf_seq.len() >= 2, "{}: expected multiple keyframes", label);
            println!("[longrun] {} ok: {} tags, {} IDRs, regressions=0, max_gap={}ms",
                label, tag_count, kf_seq.len(), max_video_gap);
        }
        for (i, buf) in sessions.iter().enumerate() {
            if buf.len() > 100_000 {
                analyze_session(buf, &format!("session{}", i));
            }
        }
        // 落盘供离线解码器（ffmpeg）复核。
        let dump = std::env::var("DTV_LONGRUN_DUMP").unwrap_or_else(|_| "/tmp/dtv_client_stream.flv".into());
        let biggest = sessions.iter().max_by_key(|b| b.len()).unwrap();
        std::fs::write(&dump, biggest).expect("dump client stream");
        println!("[longrun] client stream dumped to {} ({} bytes)", dump, biggest.len());
    }
}

#[cfg(test)]
mod live_tests_all_platforms {
    use futures_util::StreamExt;
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
    use futures_util::StreamExt;
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
    use futures_util::StreamExt;
    use super::*;
    use super::tests::spawn_test_server;

    /// Live reproduction of the reported bilibili symptom: while a Douyu stream
    /// is playing, switch to bilibili exactly like the app does
    /// (fetch_bilibili_stream_core with proxy ctx). The pump MUST switch source
    /// within seconds (a commit_new_source generation bump), and the new client
    /// MUST start from a real FLV header -- before the fix the pump kept
    /// streaming Douyu until its natural ~300s EOF and then dropped the
    /// bilibili header as a "duplicate", producing 解码错误 panels.
    ///
    /// Lenient skip when the bilibili test room happens to be offline.
    /// Room override: DTV_TEST_BILI_ROOM.
    #[actix_web::test]
    #[ignore]
    async fn live_douyu_to_bilibili_switch_is_immediate() {
        let _ = env_logger::builder().is_test(true).try_init();

        // 1) Douyu playing through the proxy.
        let douyu_url = crate::platforms::douyu::get_stream_url("9999", None)
            .await
            .expect("douyu url");
        let store = StreamUrlStore::default();
        *store.url.lock().unwrap() = douyu_url;
        *store.platform.lock().unwrap() = Some("douyu".into());
        *store.room_id.lock().unwrap() = Some("9999".into());
        *store.quality.lock().unwrap() = Some("原画".into());
        store.generation.fetch_add(1, Ordering::SeqCst);
        reset_pump_header();
        spawn_test_server(store.clone(), 39937).await;

        let client = Client::builder().no_proxy().build().unwrap();
        let ra = client.get("http://127.0.0.1:39937/live.flv").send().await.expect("get douyu");
        let mut sa = ra.bytes_stream();
        let mut head = Vec::new();
        let dl = tokio::time::Instant::now() + Duration::from_secs(10);
        while head.len() < 100_000 && tokio::time::Instant::now() < dl {
            match tokio::time::timeout(Duration::from_millis(600), sa.next()).await {
                Ok(Some(Ok(b))) => head.extend_from_slice(&b),
                _ => break,
            }
        }
        assert!(head.starts_with(b"FLV") && head.len() >= 100_000,
            "douyu baseline playback failed ({} bytes)", head.len());
        println!("[bili-switch] douyu baseline flowing ({} bytes)", head.len());

        // 2) Switch to bilibili exactly like the frontend/Rust command does.
        let bili_room = std::env::var("DTV_TEST_BILI_ROOM").unwrap_or_else(|_| "3860590".to_string());
        let gen_before = store.generation.load(Ordering::SeqCst);
        let ctx = crate::platforms::bilibili::stream_url::BiliProxyCtx {
            store: store.clone(),
            proxy_handle: Arc::new(StdMutex::new(None)),
        };
        let started = std::time::Instant::now();
        let info = crate::platforms::bilibili::stream_url::fetch_bilibili_stream_core(
            bili_room.clone(),
            "原画".to_string(),
            None,
            Some(ctx),
        )
        .await
        .expect("bilibili core");
        if info.status != Some(1) || info.stream_url.is_none() {
            println!("[bili-switch] SKIP: room {} not live/usable now ({:?} {:?})",
                bili_room, info.status, info.error_message);
            return;
        }
        let proxy_url = info.stream_url.clone().unwrap();
        println!("[bili-switch] switched to {} in {:.1}s", proxy_url, started.elapsed().as_secs_f32());

        assert_eq!(store.generation.load(Ordering::SeqCst), gen_before + 1,
            "bilibili path must bump the generation (commit_new_source)");
        let up = store.url.lock().unwrap().clone();
        assert!(up.contains("bilivideo") || up.contains("hdslb") || up.contains("bili"),
            "store should hold a bilibili upstream, got: {}", &up[..up.len().min(80)]);

        // 3) The new client must get a real FLV header + bilibili data FAST.
        let mut got = Vec::new();
        let switch_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let rb = client.get(&proxy_url).send().await.expect("get bili proxy");
        let mut sb = rb.bytes_stream();
        while got.len() < 300_000 && tokio::time::Instant::now() < switch_deadline {
            match tokio::time::timeout(Duration::from_millis(700), sb.next()).await {
                Ok(Some(Ok(b))) => got.extend_from_slice(&b),
                Ok(Some(Err(e))) => panic!("bili proxy stream error: {:#}", e),
                Ok(None) => break,
                Err(_) => {
                    if got.len() > 0 { break; }
                    continue;
                }
            }
        }
        let elapsed = started.elapsed().as_secs_f32();
        println!("[bili-switch] client got {} bytes in {:.1}s after switch", got.len(), elapsed);
        assert!(got.starts_with(b"FLV"),
            "bilibili client MUST receive the FLV header (not be treated as duplicate): first bytes = {:?}",
            &got[..got.len().min(8)]);
        assert!(got.len() >= 200_000, "bilibili stream not flowing ({} bytes)", got.len());
        assert!(elapsed < 60.0, "switch took too long: {:.1}s", elapsed);
        println!("[bili-switch] PASS: douyu->bilibili immediate switch with intact FLV header");
    }

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

#[cfg(test)]
mod live_config_probe {
    //! 探针：同时开多条斗鱼连接，比较各自的音/视频序列头（SPS/PPS、
    //! AudioSpecificConfig）。若不同 CDN 边缘下发不同配置，续流时丢弃序列头
    //! 就会让解码器拿旧配置解新 GOP -> 5103 解码错误。
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn live_douyu_config_headers_across_connections() {
        let room = std::env::var("DTV_TEST_ROOM").unwrap_or_else(|_| "6512".into());
        let client = Client::builder().no_proxy().build().unwrap();
        let mut handles = Vec::new();
        for i in 0..4 {
            let room = room.clone();
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                let url = crate::platforms::douyu::get_stream_url_with_quality(&room, "原画", None)
                    .await
                    .expect("sign");
                let resp = client.get(&url).send().await.expect("connect");
                let mut stream = resp.bytes_stream();
                use futures_util::StreamExt;
                let mut demux = flv_relay::FlvDemuxer::default();
                let mut vseq: Option<bytes::Bytes> = None;
                let mut aseq: Option<bytes::Bytes> = None;
                let mut host = String::new();
                while let Some(Ok(chunk)) = stream.next().await {
                    if host.is_empty() {
                        host = url.split('/').nth(2).unwrap_or("?").to_string();
                    }
                    demux.push(&chunk);
                    demux.take_header();
                    while let Some(tag) = demux.next_tag() {
                        if tag.is_sequence_header {
                            match tag.tag_type {
                                flv_relay::TAG_TYPE_VIDEO if vseq.is_none() => vseq = Some(tag.raw),
                                flv_relay::TAG_TYPE_AUDIO if aseq.is_none() => aseq = Some(tag.raw),
                                _ => {}
                            }
                        }
                    }
                    if vseq.is_some() && aseq.is_some() {
                        break;
                    }
                }
                (i, host, vseq, aseq)
            }));
        }
        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }
        for (i, host, v, a) in &results {
            println!(
                "[probe] conn#{} host={} vseq={} bytes {:?} aseq={} bytes {:?}",
                i,
                host,
                v.as_ref().map(|b| b.len()).unwrap_or(0),
                v.as_ref().map(|b| b[..b.len().min(48)].to_vec()),
                a.as_ref().map(|b| b.len()).unwrap_or(0),
                a.as_ref().map(|b| b[..b.len().min(24)].to_vec())
            );
        }
        // 更关键的探针：两条连接在相同时间戳上的 tag 字节是否完全一致？
        // 若不同边缘的 GOP 结构/编码不同，续流拼接就会破坏 H.264 参考链
        // （ffmpeg 的 illegal modification_of_pic_nums_idc / Reference N >= 2）。
        {
            let room = room.clone();
            let client = client.clone();
            let collect = |client: Client, room: String| async move {
                let url = crate::platforms::douyu::get_stream_url_with_quality(&room, "原画", None)
                    .await
                    .expect("sign");
                let resp = client.get(&url).send().await.expect("connect");
                let mut stream = resp.bytes_stream();
                use futures_util::StreamExt;
                let mut demux = flv_relay::FlvDemuxer::default();
                let mut map: std::collections::BTreeMap<u32, u64> = Default::default();
                let mut first_ts: Option<u32> = None;
                while let Some(Ok(chunk)) = stream.next().await {
                    demux.push(&chunk);
                    demux.take_header();
                    while let Some(tag) = demux.next_tag() {
                        if tag.tag_type == flv_relay::TAG_TYPE_VIDEO && !tag.is_sequence_header {
                            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                            for b in &tag.raw {
                                h ^= *b as u64;
                                h = h.wrapping_mul(0x0000_0100_0000_01b3);
                            }
                            first_ts.get_or_insert(tag.timestamp);
                            map.insert(tag.timestamp, h);
                        }
                    }
                    if map.len() > 400 {
                        break;
                    }
                }
                (first_ts.unwrap_or(0), map)
            };
            let (t0, m0) = collect(client.clone(), room.clone()).await;
            let (t1, m1) = collect(client.clone(), room).await;
            let lo = t0.max(t1);
            let common: Vec<u32> = m0.keys().copied().filter(|k| *k >= lo && m1.contains_key(k)).collect();
            let same = common.iter().filter(|k| m0.get(*k) == m1.get(*k)).count();
            println!(
                "[probe] frame-bytes compare: common ts={} identical={} differ={} (first ts {} vs {})",
                common.len(),
                same,
                common.len() - same,
                t0,
                t1
            );
            for k in common.iter().filter(|k| m0.get(*k) != m1.get(*k)).take(5) {
                println!("[probe]   ts={} differs", k);
            }
        }
        let v0 = results[0].2.clone();
        let a0 = results[0].3.clone();
        for (i, host, v, a) in &results[1..] {
            println!(
                "[probe] conn#{} ({}) video config same as conn#0: {} ; audio same: {}",
                i + 1,
                host,
                v == &v0,
                a == &a0
            );
        }
    }
}
