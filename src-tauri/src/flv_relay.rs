//! 斗鱼直链续流（ported & adapted from upstream `chen-zeong/DTV` @4444c9c).
//!
//! 斗鱼下发给第三方的 flv 直链寿命固定 300 秒，到点 CDN 主动 EOF，与网络、
//! 主播、CDN、清晰度无关（实测各线路误差 < 0.5s）。官方网页端每 ~300 秒重新
//! 调一次取流接口续签。续接之所以成立，是因为斗鱼 flv tag 用的是推流会话的
//! **绝对时间戳**：重新取流后时间轴延续而非归零，因此无需改写时间戳，只要
//! 丢掉新流的文件头与初始化 tag（onMetaData / sequence header），并从第一个
//! 时间戳越过已转发位置的关键帧接上即可。
//!
//! 与上游不同点：本项目的代理是**单 pump + broadcast** 架构（多个客户端共享
//! 一条上游连接，并给迟到客户端回放 primer），所以这里只提供解析/时间轴/取流
//! 原语，实际的续流循环写在 `proxy.rs` 的 pump 里。
//!
//! 为什么必须做时间轴去重：CDN 在新连接上会**回吐一个 GOP 的历史缓冲**，其
//! 时间戳小于我们已经转发出去的位置。若不丢弃，播放器会看到时间戳倒退，通常
//! 直接报解码错误 / 显示「重播」——这正是「过几分钟就要刷新」的成因。

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::Client;
use std::time::{Duration, Instant};

/// 斗鱼直链的硬性寿命。
pub const STREAM_TTL: Duration = Duration::from_secs(300);
/// 提前多久开始建中继连接。实测重新签名 + 建连 ~1~2s，8 秒余量足够；
/// 留得过长会让中继在切换前白积压数据（每 1s 约 400KB）。
pub const RENEW_LEAD: Duration = Duration::from_secs(8);
/// 预取失败后的重试间隔。
pub const RETRY_DELAY: Duration = Duration::from_millis(500);
/// 后台中继最长等待时间：EOF 时若中继仍未就绪，超过这个时间就自己同步开一条，
/// 不能让客户端干等着画面冻结。
pub const STAGE_TIMEOUT: Duration = Duration::from_secs(25);
/// 连续取流失败多少次后放弃。
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// 解析缓冲的上限。单个 FLV tag 远小于此值，超过即说明字节流无法按 FLV 解析。
pub const MAX_PENDING_BUFFER: usize = 8 * 1024 * 1024;

pub const TAG_TYPE_AUDIO: u8 = 8;
pub const TAG_TYPE_VIDEO: u8 = 9;
pub const TAG_TYPE_SCRIPT: u8 = 18;

/// 重新取流所需的上下文。仅斗鱼会设置，其余平台为 None 并因此保持直通行为。
#[derive(Clone, Debug)]
pub struct StreamRenewContext {
    pub room_id: String,
    pub quality: String,
    pub line: Option<String>,
}

#[derive(Debug)]
pub struct FlvTag {
    /// 完整 tag 字节：11 字节 tag 头 + data + 4 字节 PreviousTagSize。
    pub raw: Bytes,
    pub tag_type: u8,
    pub timestamp: u32,
    pub is_keyframe: bool,
    pub is_sequence_header: bool,
}

/// 增量 FLV 解析：喂入任意大小的字节块，按需吐出完整 tag。
#[derive(Default)]
pub struct FlvDemuxer {
    buf: BytesMut,
    header: Option<Bytes>,
    header_parsed: bool,
}

impl FlvDemuxer {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// 解析文件头（"FLV" + 版本 + 标志 + dataOffset，后随 PreviousTagSize0）。
    /// 返回 false 表示数据还不够，下次再试。
    fn try_parse_header(&mut self) -> bool {
        if self.header_parsed {
            return true;
        }
        if self.buf.len() < 13 {
            return false;
        }
        if &self.buf[..3] == b"FLV" {
            let data_offset =
                u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize;
            let total = data_offset.saturating_add(4);
            if self.buf.len() < total {
                return false;
            }
            self.header = Some(self.buf.split_to(total).freeze());
        }
        self.header_parsed = true;
        true
    }

    /// 必须自行触发解析：调用方会先取文件头再取 tag。返回 None 表示本段没有
    /// （或已经取过）文件头。
    pub fn take_header(&mut self) -> Option<Bytes> {
        self.try_parse_header();
        self.header.take()
    }

    /// 缓冲越积越多却吐不出 tag，说明上游不是能按 FLV 解析的字节流。
    pub fn is_desynced(&self) -> bool {
        self.buf.len() > MAX_PENDING_BUFFER
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.header = None;
        self.header_parsed = false;
    }

    pub fn next_tag(&mut self) -> Option<FlvTag> {
        if !self.try_parse_header() {
            return None;
        }
        if self.buf.len() < 11 {
            return None;
        }
        let data_size =
            ((self.buf[1] as usize) << 16) | ((self.buf[2] as usize) << 8) | (self.buf[3] as usize);
        let total = 11 + data_size + 4;
        if self.buf.len() < total {
            return None;
        }

        let tag_type = self.buf[0] & 0x1f;
        // 时间戳是 3 字节小端序拼 1 字节高位扩展。
        let timestamp = ((self.buf[7] as u32) << 24)
            | ((self.buf[4] as u32) << 16)
            | ((self.buf[5] as u32) << 8)
            | (self.buf[6] as u32);

        let (is_keyframe, is_sequence_header) = if data_size >= 2 {
            let first = self.buf[11];
            let second = self.buf[12];
            match tag_type {
                TAG_TYPE_VIDEO => (first >> 4 == 1, second == 0),
                TAG_TYPE_AUDIO => (false, second == 0),
                _ => (false, false),
            }
        } else {
            (false, false)
        };

        let raw = self.buf.split_to(total).freeze();
        Some(FlvTag { raw, tag_type, timestamp, is_keyframe, is_sequence_header })
    }
}

/// 记录已转发到的时间轴位置，用于在接续时丢弃新流回吐的重复 tag。
/// 音视频分开记录，避免交错导致误丢。
#[derive(Default)]
pub struct Timeline {
    last_video_ts: Option<u32>,
    last_audio_ts: Option<u32>,
}

impl Timeline {
    pub fn observe(&mut self, tag: &FlvTag) {
        match tag.tag_type {
            TAG_TYPE_VIDEO => self.last_video_ts = Some(tag.timestamp),
            TAG_TYPE_AUDIO => self.last_audio_ts = Some(tag.timestamp),
            _ => {}
        }
    }

    /// 新流的 tag 是否已经越过旧流转发到的位置。
    pub fn is_ahead(&self, tag: &FlvTag) -> bool {
        let last = match tag.tag_type {
            TAG_TYPE_VIDEO => self.last_video_ts,
            TAG_TYPE_AUDIO => self.last_audio_ts,
            _ => None,
        };
        match last {
            Some(prev) => tag.timestamp > prev,
            None => true,
        }
    }

    pub fn last_video(&self) -> Option<u32> {
        self.last_video_ts
    }

    pub fn last_audio(&self) -> Option<u32> {
        self.last_audio_ts
    }

    /// 在切换点处把视频时间轴推进到 cut，使新段从 cut 开始的 tag 不会被当成
    /// 「已转发过」而误丢（也保证后续 CDN 回吐的重复帧仍会被丢掉）。
    /// 无缝切换之后一次性对齐音视频位置到「已经转发到客户端」的地方。
    pub fn set_positions(&mut self, video_ts: u32, audio_ts: u32) {
        self.last_video_ts = Some(self.last_video_ts.map_or(video_ts, |p| p.max(video_ts)));
        self.last_audio_ts = Some(self.last_audio_ts.map_or(audio_ts, |p| p.max(audio_ts)));
    }

    pub fn set_last_video(&mut self, ts: u32) {
        self.last_video_ts = Some(match self.last_video_ts {
            Some(prev) => prev.max(ts),
            None => ts,
        });
    }
}

/// 一条上游流段的读取状态。
pub struct Segment {
    pub started_at: Instant,
    /// 是否为接续段（首段原样转发，接续段需要按 tag 裁剪）。
    pub continuation: bool,
    /// 接续段在遇到第一个可用关键帧之前不转发任何内容。
    pub waiting_for_keyframe: bool,
}

impl Segment {
    pub fn new(continuation: bool) -> Self {
        Self::new_with_preage(continuation, Duration::ZERO)
    }

    /// 新段的连接可能在切换前就已经活了 `preage`（后台中继提前建连），
    /// 预取时刻必须把这段年龄算进去，否则第二次续流会等到旧连接已经 EOF
    /// 才开始建连 —— 那 1 秒多的空档就是「转圈」。
    pub fn new_with_preage(continuation: bool, preage: Duration) -> Self {
        let started_at = Instant::now().checked_sub(preage).unwrap_or_else(Instant::now);
        Self { started_at, continuation, waiting_for_keyframe: continuation }
    }

    /// 当前段已经跑了多久（日志/诊断用）。
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn should_prefetch(&self) -> bool {
        self.started_at.elapsed() + RENEW_LEAD >= STREAM_TTL
    }
}

pub type UpstreamBody =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// 上游段类型别名，便于在 proxy.rs 中命名。
pub type BoxedBytesStream = UpstreamBody;

/// 重新取流：重新签名 + 建连，返回可直接读取的字节流。
/// 预取与兜底重连共用。
pub async fn acquire_douyu_stream(
    ctx: &StreamRenewContext,
    client: &Client,
) -> Result<(UpstreamBody, String), String> {
    let url = crate::platforms::douyu::get_stream_url_with_quality(
        &ctx.room_id,
        &ctx.quality,
        ctx.line.as_deref(),
    )
    .await
    .map_err(|e| format!("重新取流失败: {}", e))?;
    let response = crate::proxy::build_upstream_request(client, &url)
        .send()
        .await
        .map_err(|e| format!("连接上游失败: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("上游返回 {}", response.status()));
    }
    // 返回新地址：调用方（pump）负责写回 store，这样投屏 get_cast_info 和
    // proxy handler 看到的都是最新签名，同时 generation 保持不变（这是同一条
    // 逻辑流的延续，而不是切房间）。
    Ok((Box::pin(response.bytes_stream()), url))
}

/// 一条「正在被后台任务持续读取」的新上游连接。
///
/// 实测事实：斗鱼新开的直链从 CDN 边缘缓存的某个关键帧开始（比直播沿落后
/// 3~6 秒），并且同样以 1x 实时速率推进；旧连接也一样落后它自己的
/// 「起播回吐深度」。两条连接的落后量互不相等，所以：
///
/// * 新连接比旧连接靠前 -> 一旦它的内容超过「已转发位置」就**立刻**切换，
///   旧连接剩下的内容直接丢弃（客户端时间轴上没有任何缺口）。
/// * 新连接比旧连接靠后 -> 只能等旧连接 EOF 再切；此时去重会把落后的部分丢掉，
///   缺口就是两条连接落后量之差（无法避免，但已经是理论下限）。
///
/// 因此这里提供 `poll()`：非阻塞地把中继通道里已到的字节搬进本地缓冲，并顺带
/// 解析出「新连接目前推进到的时间戳」，让 pump 能在不阻塞转发的前提下判断
/// 是否可以立刻切换。
pub struct StagedSegment {
    /// 新直链，需要写回 store（投屏/handler 才能看到最新地址）。
    pub url: String,
    /// 后台中继通道：切换前排空积压，切换后继续从这里读。
    pub body: SourceBody,
    /// 中继开始的时间（含重新签名 + 建连）：既是日志用的连接年龄，
    /// 也是下一段预取时刻的计算基准。
    pub relay_started_at: Instant,
    /// pump 接手后置 true：此后这条连接就是直播本流，积压上限必须失效。
    pub consumed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    buffered: BytesMut,
    parsed: usize,
    peek: FlvDemuxer,
    video_ts: Option<u32>,
    audio_ts: Option<u32>,
    /// 中继任务已经结束（上游断开或积压上限）且通道已排空。
    done: bool,
}

impl StagedSegment {
    pub fn new(url: String, body: SourceBody, relay_started_at: Instant) -> Self {
        Self {
            url,
            body,
            relay_started_at,
            consumed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            buffered: BytesMut::new(),
            parsed: 0,
            peek: FlvDemuxer::default(),
            video_ts: None,
            audio_ts: None,
            done: false,
        }
    }

    /// 中继是否已经结束（且积压已排空）：pump 据此丢弃这条废连接、重新预取。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 非阻塞搬运 + 增量解析。可以在 pump 每个 chunk 上调用（只解析新增部分）。
    pub fn poll(&mut self) {
        if let SourceBody::Chan(rx) = &mut self.body {
            while let Ok(b) = rx.try_recv() {
                self.buffered.extend_from_slice(&b);
                if self.buffered.len() > MAX_PENDING_BUFFER {
                    break;
                }
            }
            // 必须在排空之后再判定：sender 掉线时通道里可能还有数据。
            if rx.is_closed() {
                self.done = true;
            }
        }
        if self.buffered.len() <= self.parsed {
            return;
        }
        let tail: Vec<u8> = self.buffered[self.parsed..].to_vec();
        self.parsed = self.buffered.len();
        self.peek.push(&tail);
        self.peek.take_header();
        while let Some(tag) = self.peek.next_tag() {
            match tag.tag_type {
                TAG_TYPE_VIDEO => {
                    self.video_ts = Some(self.video_ts.map_or(tag.timestamp, |v| v.max(tag.timestamp)));
                }
                TAG_TYPE_AUDIO => {
                    self.audio_ts = Some(self.audio_ts.map_or(tag.timestamp, |a| a.max(tag.timestamp)));
                }
                _ => {}
            }
        }
    }

    /// 新连接目前已推进到的视频/音频时间戳（None = 还没有完整 tag）。
    pub fn video_ts(&self) -> Option<u32> {
        self.video_ts
    }
    pub fn audio_ts(&self) -> Option<u32> {
        self.audio_ts
    }

    /// 取走已积压的全部字节（切换瞬间用；pump 会用自己的 demuxer 重新按 tag 去重）。
    pub fn take_buffered(&mut self) -> Bytes {
        self.parsed = 0;
        std::mem::take(&mut self.buffered).freeze()
    }
}

/// 上游字节的两种来源：首段直接读 HTTP 响应；续流段读后台中继通道。
pub enum SourceBody {
    Http(UpstreamBody),
    Chan(tokio::sync::mpsc::UnboundedReceiver<Bytes>),
}

impl SourceBody {
    /// 取下一块上游字节；通道关闭/上游结束返回 None。
    pub async fn next_chunk(&mut self) -> Option<Bytes> {
        match self {
            SourceBody::Http(b) => match b.next().await {
                Some(Ok(c)) => Some(c),
                Some(Err(e)) => {
                    eprintln!("[flv_relay] upstream chunk error: {}", e);
                    None
                }
                None => None,
            },
            SourceBody::Chan(rx) => rx.recv().await,
        }
    }

    /// 非阻塞排空当前已积压的字节（切换瞬间使用）。
    pub fn drain_available(&mut self) -> Bytes {
        let SourceBody::Chan(rx) = self else { return Bytes::new() };
        let mut out = BytesMut::new();
        while let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&b);
            if out.len() > MAX_PENDING_BUFFER {
                break;
            }
        }
        out.freeze()
    }
}

/// pump 迟迟不切换时（例如房间已切走），中继最多积压这么多字节后就自己停下。
/// 注意：**切换之后这条连接就是直播本流**，这两个上限自动失效（consumed 标记），
/// 否则会把正在看的流每十几秒掐断一次 —— 之前实测就是这样，表现为反复重签、
/// 带宽翻倍、每次切换都要重吃一段 CDN 回吐缓冲。
pub const RELAY_CAP_BYTES: usize = 48 * 1024 * 1024;
/// 中继未被接手时的最长存活时间，兜底防止任务泄漏 / 内存无上界。
pub const RELAY_CAP_SECS: u64 = 120;

/// 建立新连接并启动后台中继。返回即可用（通道里会陆续积压字节）。
pub async fn open_staged_relay(
    ctx: &StreamRenewContext,
    client: &Client,
) -> Result<StagedSegment, String> {
    // 连接年龄从这里算起（含重新签名 + 建连），pump 切换时用它推算下一次预取时刻。
    let started = Instant::now();
    let (body, url) = acquire_douyu_stream(ctx, client).await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let consumed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let consumed_task = consumed.clone();
    tokio::spawn(async move {
        let mut body = body;
        let mut relayed = 0usize;
        loop {
            // 未被 pump 接手前限制积压（防止白烧带宽/内存）；一旦接手，
            // 这条连接就是直播本流，只跟着上游走，直到它自然结束。
            if !consumed_task.load(std::sync::atomic::Ordering::Relaxed)
                && (started.elapsed() >= Duration::from_secs(RELAY_CAP_SECS)
                    || relayed >= RELAY_CAP_BYTES)
            {
                break;
            }
            match body.next().await {
                Some(Ok(chunk)) => {
                    relayed += chunk.len();
                    // 通道被接收端丢弃（pump 放弃续流/房间切换）时 send 失败，任务自然退出。
                    if tx.send(chunk).is_err() {
                        break;
                    }
                }
                Some(Err(e)) => {
                    eprintln!("[flv_relay] relay upstream error after {} bytes: {}", relayed, e);
                    break;
                }
                None => break,
            }
        }
        // 关闭发送端：pump 排空积压后 recv 会得到 None，按 EOF 处理。
        drop(tx);
    });
    let mut st = StagedSegment::new(url, SourceBody::Chan(rx), started);
    st.consumed = consumed;
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼出一个最小 FLV tag：11 字节头 + data + 4 字节 PreviousTagSize。
    fn make_tag(tag_type: u8, timestamp: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(tag_type);
        let size = data.len() as u32;
        out.extend_from_slice(&size.to_be_bytes()[1..]);
        out.extend_from_slice(&timestamp.to_be_bytes()[1..]);
        out.push((timestamp >> 24) as u8);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(data);
        out.extend_from_slice(&(11 + size).to_be_bytes());
        out
    }

    fn flv_file_header() -> Vec<u8> {
        let mut h = b"FLV\x01\x05".to_vec();
        h.extend_from_slice(&9u32.to_be_bytes()); // dataOffset = 9
        h.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
        h
    }

    /// Demuxer must emit the file header once, then whole tags, and handle tags
    /// split arbitrarily across chunk boundaries.
    #[test]
    fn demuxer_parses_header_then_whole_tags_across_chunks() {
        let mut bytes = flv_file_header();
        bytes.extend_from_slice(&make_tag(TAG_TYPE_SCRIPT, 0, b"\x02\x00\x0aonMetaData"));
        bytes.extend_from_slice(&make_tag(TAG_TYPE_VIDEO, 0, &[0x17, 0x00, 0, 0, 0, 1, 2]));
        bytes.extend_from_slice(&make_tag(TAG_TYPE_VIDEO, 1000, &[0x17, 0x01, 0, 0x03, 0xe8, 9, 9]));
        bytes.extend_from_slice(&make_tag(TAG_TYPE_AUDIO, 1010, &[0xAF, 0x01, 7, 7]));

        let mut demux = FlvDemuxer::default();
        let mut tags = Vec::new();
        // Feed 5 bytes at a time so nearly every tag straddles a boundary.
        for chunk in bytes.chunks(5) {
            demux.push(chunk);
            while let Some(t) = demux.next_tag() {
                tags.push(t);
            }
        }
        assert_eq!(tags.len(), 4, "expected script + vseq + keyframe + audio");
        assert_eq!(tags[0].tag_type, TAG_TYPE_SCRIPT);
        assert!(tags[1].is_sequence_header, "0x17/0x00 is the AVC sequence header");
        assert!(tags[2].is_keyframe && !tags[2].is_sequence_header);
        assert_eq!(tags[2].timestamp, 1000);
        assert_eq!(tags[3].tag_type, TAG_TYPE_AUDIO);
        assert_eq!(tags[3].timestamp, 1010);
        assert!(!demux.is_desynced());
    }

    /// The file header must be consumable separately, and only once.
    #[test]
    fn demuxer_takes_header_once() {
        let mut bytes = flv_file_header();
        bytes.extend_from_slice(&make_tag(TAG_TYPE_VIDEO, 0, &[0x17, 0x01, 0, 0, 0, 1]));
        let mut demux = FlvDemuxer::default();
        demux.push(&bytes);
        let h = demux.take_header().expect("header");
        assert!(h.starts_with(b"FLV"));
        assert_eq!(h.len(), 13);
        assert!(demux.take_header().is_none(), "header must not be returned twice");
        assert!(demux.next_tag().is_some());
    }

    /// Douyu renewal must drop the CDN's back-buffer: a spliced segment replays
    /// a GOP whose timestamps are <= what we already forwarded, which would make
    /// the player see time go backwards (decode error / "重播" prompt).
    #[test]
    fn timeline_rejects_replayed_gop_and_accepts_new_frames() {
        let mut tl = Timeline::default();
        let old1 = tag_from(TAG_TYPE_VIDEO, 290_000, &[0x17, 0x01]);
        let old2 = tag_from(TAG_TYPE_VIDEO, 295_000, &[0x27, 0x01]);
        let old_a = tag_from(TAG_TYPE_AUDIO, 295_500, &[0xAF, 0x01]);
        tl.observe(&old1);
        tl.observe(&old2);
        tl.observe(&old_a);
        assert_eq!(tl.last_video(), Some(295_000));

        // New segment re-sends the tail of the previous GOP: must be dropped.
        assert!(!tl.is_ahead(&tag_from(TAG_TYPE_VIDEO, 292_000, &[0x27, 0x01])));
        assert!(!tl.is_ahead(&tag_from(TAG_TYPE_VIDEO, 295_000, &[0x27, 0x01])));
        // ...and audio is tracked independently.
        assert!(!tl.is_ahead(&tag_from(TAG_TYPE_AUDIO, 294_000, &[0xAF, 0x01])));
        assert!(tl.is_ahead(&tag_from(TAG_TYPE_AUDIO, 296_000, &[0xAF, 0x01])));
        // New video beyond the timeline is accepted.
        assert!(tl.is_ahead(&tag_from(TAG_TYPE_VIDEO, 296_000, &[0x27, 0x01])));
        // Script tags have no timeline entry: always "ahead" (caller drops them).
        assert!(tl.is_ahead(&tag_from(TAG_TYPE_SCRIPT, 0, b"m")));
    }

    fn tag_from(tag_type: u8, timestamp: u32, data: &[u8]) -> FlvTag {
        let raw = make_tag(tag_type, timestamp, data);
        let (is_keyframe, is_sequence_header) = if tag_type == TAG_TYPE_VIDEO && raw.len() > 12 {
            (raw[11] >> 4 == 1, raw[12] == 0)
        } else {
            (false, tag_type == TAG_TYPE_AUDIO && raw.len() > 12 && raw[12] == 0)
        };
        FlvTag {
            raw: Bytes::from(raw),
            tag_type,
            timestamp,
            is_keyframe,
            is_sequence_header,
        }
    }

    /// Prefetch must be scheduled ~5s before the 300s TTL, not after EOF.
    #[test]
    fn segment_prefetches_before_expiry() {
        let seg = Segment::new(false);
        assert!(!seg.should_prefetch(), "brand new segment must not prefetch");
        assert!(!seg.continuation && !seg.waiting_for_keyframe);
        let cont = Segment::new(true);
        assert!(cont.continuation && cont.waiting_for_keyframe,
            "a continuation segment must withhold output until a keyframe");
        assert_eq!(STREAM_TTL, Duration::from_secs(300));
        assert!(RENEW_LEAD < STREAM_TTL);
    }

    /// Non-FLV input must not wedge memory: the desync guard trips so the caller
    /// can stop tracking the timeline (and forward raw bytes instead). A non-FLV
    /// prefix must never be reported as a file header.
    #[test]
    fn demuxer_flags_desync_on_garbage() {
        let mut demux = FlvDemuxer::default();
        demux.push(&vec![0xAA; MAX_PENDING_BUFFER + 1]);
        assert!(demux.is_desynced(), "runaway buffer must be flagged as desynced");
        assert!(demux.take_header().is_none(), "garbage must not yield an FLV header");
        demux.reset();
        assert!(!demux.is_desynced());
        assert!(demux.take_header().is_none());
    }

    /// 中继切换的去重必须保证输出严格递增且不重复。
    /// 实测场景：旧流转发到 v=1_000_033，中继通道里积压的内容从更早的 CDN
    /// 回吐关键帧开始（v=999_000 一带）—— 直接转发会让时间戳倒退
    /// （MSE 拒收 / 播放器停机）。按轨去重后剩下的部分必须无缝接在后面。
    #[test]
    fn relay_drain_dedup_keeps_timestamps_monotonic() {
        fn dedup_track(
            tags: &[(u8, u32)],
            last_video: &mut u32,
            last_audio: &mut u32,
        ) -> Vec<(u8, u32)> {
            let mut kept = Vec::new();
            for &(kind, ts) in tags {
                let dup = match kind {
                    TAG_TYPE_AUDIO => {
                        if ts <= *last_audio { true } else { *last_audio = ts; false }
                    }
                    TAG_TYPE_VIDEO => {
                        if ts <= *last_video { true } else { *last_video = ts; false }
                    }
                    _ => true, // 脚本/序列头只认第一次
                };
                if !dup { kept.push((kind, ts)); }
            }
            kept
        }

        let (mut last_v, mut last_a) = (1_000_033u32, 1_000_010u32);

        // 中继积压内容：CDN 回吐的历史（早于标记，必须全部丢掉）+ 直播沿之后的新内容。
        let mut drained: Vec<(u8, u32)> = Vec::new();
        drained.push((TAG_TYPE_SCRIPT, 0));
        for ts in (999_000..1_000_033).step_by(33) {
            drained.push((TAG_TYPE_VIDEO, ts));
            drained.push((TAG_TYPE_AUDIO, ts + 5));
        }
        for ts in (1_000_033..1_001_200).step_by(33) {
            drained.push((TAG_TYPE_VIDEO, ts));
            drained.push((TAG_TYPE_AUDIO, ts + 5));
        }
        let kept = dedup_track(&drained, &mut last_v, &mut last_a);

        let video: Vec<u32> = kept.iter().filter(|(k, _)| *k == TAG_TYPE_VIDEO).map(|(_, t)| *t).collect();
        let audio: Vec<u32> = kept.iter().filter(|(k, _)| *k == TAG_TYPE_AUDIO).map(|(_, t)| *t).collect();
        assert!(!video.is_empty() && !audio.is_empty(), "relay drain kept nothing -> hole");
        for w in video.windows(2) { assert!(w[1] > w[0], "video went backwards: {} -> {}", w[0], w[1]); }
        for w in audio.windows(2) { assert!(w[1] > w[0], "audio went backwards: {} -> {}", w[0], w[1]); }
        assert!(kept.iter().all(|(k, _)| *k != TAG_TYPE_SCRIPT), "onMetaData must not be replayed");
        assert!(video[0] > 1_000_033, "first kept video must be ahead of the old stream");
        assert_eq!(video[0], 1_000_066, "must resume on the very next frame (no hole)");
        assert_eq!(last_v, *video.last().unwrap());
        assert_eq!(last_a, *audio.last().unwrap());

        // 第二次续流：同样的规则，标记继续推进。
        let drained2: Vec<(u8, u32)> = (last_v.saturating_sub(300)..last_v + 900)
            .step_by(33)
            .flat_map(|ts| [(TAG_TYPE_VIDEO, ts), (TAG_TYPE_AUDIO, ts + 5)])
            .collect();
        let keep2 = dedup_track(&drained2, &mut last_v, &mut last_a);
        let video2: Vec<u32> = keep2.iter().filter(|(k, _)| *k == TAG_TYPE_VIDEO).map(|(_, t)| *t).collect();
        for w in video2.windows(2) { assert!(w[1] > w[0], "second renewal rewound video"); }
        assert!(video2.first().map_or(false, |t| *t > *video.last().unwrap()),
            "second renewal must continue after the first");
    }
}
