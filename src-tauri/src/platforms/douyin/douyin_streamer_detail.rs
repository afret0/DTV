use crate::platforms::common::http_client::HttpClient;
use crate::platforms::common::types::StreamVariant;
use crate::platforms::common::GetStreamUrlPayload;
use crate::platforms::common::LiveStreamInfo as CommonLiveStreamInfo;
use crate::platforms::douyin::web_api::{
    choose_flv_stream, fetch_room_data, normalize_douyin_live_id, DouyinRoomData,
    DEFAULT_USER_AGENT,
};
use crate::proxy::ProxyServerHandle;
use crate::StreamUrlStore;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::Value;
use tauri::{command, AppHandle, State};

const QUALITY_OD: &str = "OD";
const QUALITY_BD: &str = "BD";
const QUALITY_UHD: &str = "UHD";
#[command]
pub async fn get_douyin_live_stream_url(
    app_handle: AppHandle,
    stream_url_store: State<'_, StreamUrlStore>,
    proxy_server_handle: State<'_, ProxyServerHandle>,
    payload: GetStreamUrlPayload,
) -> Result<CommonLiveStreamInfo, String> {
    get_douyin_live_stream_url_with_quality(
        app_handle,
        stream_url_store,
        proxy_server_handle,
        payload,
        QUALITY_OD.to_string(),
    )
    .await
}

pub async fn fetch_douyin_stream_core(
    requested_id: String,
    quality: String,
) -> Result<CommonLiveStreamInfo, String> {
    if requested_id.is_empty() {
        return Ok(CommonLiveStreamInfo {
            title: None,
            anchor_name: None,
            avatar: None,
            stream_url: None,
            status: None,
            error_message: Some("Douyin web_id cannot be empty.".to_string()),
            upstream_url: None,
            available_streams: None,
            normalized_room_id: None,
            web_rid: None,
        });
    }

    println!(
        "[Douyin Stream Detail] Fetching stream for '{}' with requested quality '{}'",
        requested_id, quality
    );

    let http_client = HttpClient::new_direct_connection()
        .map_err(|e| format!("Failed to create direct connection HttpClient: {}", e))?;

    let normalized_id = normalize_douyin_live_id(&requested_id);
    let DouyinRoomData { mut room } = fetch_room_data(&http_client, &normalized_id, None).await?;
    let origin_from_html = fetch_origin_flv_from_live_page(&http_client, &normalized_id)
        .await
        .unwrap_or_else(|err| {
            println!(
                "[Douyin Stream Detail] Failed to fetch live page for origin stream: {}",
                err
            );
            None
        });
    if let Some(url) = origin_from_html.as_deref() {
        insert_origin_flv(&mut room, url);
    }
    let room_id_str = room
        .get("id_str")
        .and_then(|v| v.as_str())
        .or_else(|| room.get("id").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| normalized_id.clone());
    let web_rid = extract_web_rid(&room).unwrap_or_else(|| normalized_id.clone());
    let status = room
        .get("status")
        .and_then(|v| v.as_i64())
        .unwrap_or_default() as i32;
    let title = room
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let anchor_name = extract_anchor_name(&room);
    let avatar = extract_avatar(&room);
    let available_streams = collect_available_streams(&room);

    if status != 2 {
        println!(
            "[Douyin Stream Detail] Room '{}' is not live (status={}). Returning metadata only.",
            web_rid, status
        );
        return Ok(CommonLiveStreamInfo {
            title,
            anchor_name,
            avatar,
            stream_url: None,
            status: Some(status),
            error_message: None,
            upstream_url: None,
            available_streams: available_streams.clone(),
            normalized_room_id: Some(room_id_str),
            web_rid: Some(web_rid),
        });
    }

    let target_quality = normalize_quality_tag(&quality);
    let selected = pick_douyin_flv_by_quality(&room, target_quality, origin_from_html.as_deref())
        .or_else(|| choose_flv_stream(&room, target_quality))
        .or_else(|| first_flv_stream(&room))
        .ok_or_else(|| {
            "[Douyin Stream Detail] No FLV streams available in stream_url.flv_pull_url".to_string()
        })?;
    let (selected_key, real_url) = selected;
    println!(
        "[Douyin Stream Detail] Selected FLV stream key='{}' url='{}'",
        selected_key, real_url
    );

    let sanitized_url = enforce_https(&real_url);

    Ok(CommonLiveStreamInfo {
        title,
        anchor_name,
        avatar,
        stream_url: Some(sanitized_url.clone()),
        status: Some(status),
        error_message: None,
        upstream_url: Some(sanitized_url),
        available_streams,
        normalized_room_id: Some(room_id_str),
        web_rid: Some(web_rid),
    })
}

#[command]
pub async fn get_douyin_live_stream_url_with_quality(
    _app_handle: AppHandle,
    _stream_url_store: State<'_, StreamUrlStore>,
    _proxy_server_handle: State<'_, ProxyServerHandle>,
    payload: GetStreamUrlPayload,
    quality: String,
) -> Result<CommonLiveStreamInfo, String> {
    fetch_douyin_stream_core(payload.args.room_id_str.trim().to_string(), quality).await
}


fn normalize_quality_tag(input: &str) -> &str {
    match input.trim().to_uppercase().as_str() {
        "OD" => QUALITY_OD,
        "BD" => QUALITY_BD,
        "UHD" => QUALITY_UHD,
        _ => QUALITY_OD,
    }
}

pub(crate) fn extract_web_rid(room: &Value) -> Option<String> {
    room.get("owner")
        .and_then(|o| o.get("web_rid"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            room.get("anchor")
                .and_then(|a| a.get("web_rid"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            room.get("web_rid")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

pub(crate) fn extract_anchor_name(room: &Value) -> Option<String> {
    room.get("anchor_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            room.get("owner")
                .and_then(|o| o.get("nickname"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            room.get("anchor")
                .and_then(|a| a.get("nickname"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

fn enforce_https(url: &str) -> String {
    if url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with("http://") {
        format!("https://{}", &url["http://".len()..])
    } else {
        url.to_string()
    }
}

pub(crate) fn extract_avatar(room: &Value) -> Option<String> {
    room.get("owner")
        .and_then(|o| o.get("avatar_thumb"))
        .and_then(|thumb| thumb.get("url_list"))
        .and_then(|list| list.get(0))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            room.get("anchor")
                .and_then(|a| a.get("avatar_thumb"))
                .and_then(|thumb| thumb.get("url_list"))
                .and_then(|list| list.get(0))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

pub(crate) fn collect_available_streams(room: &Value) -> Option<Vec<StreamVariant>> {
    let flv_map = room
        .get("stream_url")
        .and_then(|v| v.get("flv_pull_url"))
        .and_then(|v| v.as_object())?;
    let variants = flv_map
        .iter()
        .filter_map(|(k, v)| {
            v.as_str().map(|url| StreamVariant {
                url: url.to_string(),
                format: Some("flv".to_string()),
                desc: Some(k.to_string()),
                qn: None,
                protocol: url.split(':').next().map(|s| s.to_string()),
            })
        })
        .collect::<Vec<_>>();
    if variants.is_empty() {
        None
    } else {
        Some(variants)
    }
}

fn first_flv_stream(room: &Value) -> Option<(String, String)> {
    let flv_map = room
        .get("stream_url")
        .and_then(|v| v.get("flv_pull_url"))
        .and_then(|v| v.as_object())?;
    flv_map
        .iter()
        .find_map(|(k, v)| v.as_str().map(|url| (k.to_string(), url.to_string())))
}

fn pick_douyin_flv_by_quality(
    room: &Value,
    target_quality: &str,
    origin_override: Option<&str>,
) -> Option<(String, String)> {
    let flv_map = room
        .get("stream_url")
        .and_then(|v| v.get("flv_pull_url"))
        .and_then(|v| v.as_object())?;

    let origin_url = origin_override
        .or_else(|| flv_map.get("ORIGIN").and_then(|v| v.as_str()));
    let full_hd1 = flv_map.get("FULL_HD1").and_then(|v| v.as_str());
    let hd1 = flv_map.get("HD1").and_then(|v| v.as_str());
    let sd_fallback = flv_map
        .get("SD1")
        .and_then(|v| v.as_str())
        .or_else(|| flv_map.get("SD2").and_then(|v| v.as_str()));

    match target_quality {
        QUALITY_OD => origin_url
            .map(|u| ("ORIGIN".to_string(), u.to_string()))
            .or_else(|| full_hd1.map(|u| ("FULL_HD1".to_string(), u.to_string()))),
        QUALITY_UHD => {
            full_hd1.map(|u| ("FULL_HD1".to_string(), u.to_string()))
        }
        QUALITY_BD => hd1
            .map(|u| ("HD1".to_string(), u.to_string()))
            .or_else(|| sd_fallback.map(|u| ("SD1".to_string(), u.to_string()))),
        _ => None,
    }
}

fn insert_origin_flv(room: &mut Value, origin_url: &str) {
    let Some(stream_url) = room.get_mut("stream_url") else {
        return;
    };
    let flv_map_val = stream_url.get_mut("flv_pull_url");
    match flv_map_val {
        Some(map_val) if map_val.is_object() => {
            if let Some(map) = map_val.as_object_mut() {
                map.insert("ORIGIN".to_string(), Value::String(origin_url.to_string()));
            }
        }
        _ => {
            let mut new_map = serde_json::Map::new();
            new_map.insert("ORIGIN".to_string(), Value::String(origin_url.to_string()));
            stream_url.as_object_mut().map(|obj| {
                obj.insert("flv_pull_url".to_string(), Value::Object(new_map))
            });
        }
    }
}

async fn fetch_origin_flv_from_live_page(
    http_client: &HttpClient,
    web_id: &str,
) -> Result<Option<String>, String> {
    let url = format!("https://live.douyin.com/{}", web_id);
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    headers.insert(
        REFERER,
        HeaderValue::from_str(&url).map_err(|e| format!("Invalid Referer header: {}", e))?,
    );

    let html = http_client
        .get_text_with_headers(&url, Some(headers))
        .await?;
    let candidates = extract_origin_flv_candidates_from_html(&html);
    if candidates.is_empty() {
        println!("[Douyin Stream Detail] live page exposed no usable FLV candidate");
        return Ok(None);
    }
    Ok(pick_live_origin_flv(http_client, candidates).await)
}

const ORIGIN_PAGE_WINDOW: usize = 20_000;
/// 横屏游戏源最好的模板不低于这个档位（hd）时才优先它；只给低清模板的横屏源
/// 会让「原画」比低档还糊，此时回退合成源的高清模板。
pub(crate) const DOUYIN_HIGH_DEF_RANK: i32 = 40;

pub(crate) fn douyin_template_rank(tpl: &str) -> i32 {
    let low = tpl.to_ascii_lowercase();
    // 低延迟转码两套前缀都见过：ls*（lssd5）与 hiq*（hiqsd5/hiquhd5）。
    let low_latency = low.contains("ls") || low.contains("hiq");
    let base = if low.contains("uhd") {
        50
    } else if tpl.is_empty() {
        45
    } else if low.contains("or4") {
        44
    } else if low.contains("hd") {
        40
    } else if low.contains("sd") {
        30
    } else if low.contains("ld") {
        20
    } else if low.contains("md") {
        10
    } else {
        35
    };
    if low_latency {
        base - 25
    } else {
        base
    }
}

pub(crate) fn douyin_template_of(url: &str) -> String {
    let Some(tail) = url.rsplit('/').next() else {
        return String::new();
    };
    let Some(stem) = tail.split('?').next() else {
        return String::new();
    };
    match stem.rsplit_once(".flv") {
        Some((head, _)) => match head.rsplit_once("stream-") {
            Some((_, rest)) => rest
                .chars()
                .skip_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .trim_start_matches('_')
                .to_string(),
            None => String::new(),
        },
        None => String::new(),
    }
}

/// 多机位/多场景房间（游戏直播常见）：官方页面同时给出「横屏游戏源」和
/// 「竖屏合成源」两套拉流地址，API 的 flv_pull_url 只给合成源（摄像头与游戏
/// 上下堆叠的竖画布 1920x1810），于是看起来「摄像头被塞进画面正中」。
/// 按 stream id 分组、读各组 sdk_params 的分辨率：横画布优先，同为横画布时取
/// 最接近 16:9 的（实测游戏源 1920x1080 完全贴合，合成源 1920x1810 近方形），
/// 那才是「游戏全屏 + 摄像头 PIP」的源；同一 id 内再按画质模板降序。
/// 返回按优先级排好的候选列表，由 `pick_live_origin_flv` 逐个验活。
fn extract_origin_flv_candidates_from_html(html: &str) -> Vec<String> {
    let re = match Regex::new(r#"https?://[^"'\s]*stream-(\d+)[^"'\s]*\.flv[^"'\s]*"#) {
        Ok(re) => re,
        Err(_) => return Vec::new(),
    };
    let res_re = match Regex::new(r#"resolution[\\"]*:\s*[\\"]*(\d+)x(\d+)"#) {
        Ok(re) => re,
        Err(_) => return Vec::new(),
    };
    struct Cand {
        url: String,
        stream_id: String,
        order: usize,
    }
    let mut cands: Vec<Cand> = Vec::new();
    for (idx, cap) in re.captures_iter(html).enumerate() {
        let m = cap.get(0).expect("whole match");
        let mut url = unescape_js_escapes(m.as_str());
        url = url.replace("&amp;", "&");
        url = url.trim_end_matches('\\').to_string();
        let host = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        // 抖音 flv 边缘既有 pull-flv-qX 也有 pull-qX（不带 flv 字样），只能用黑名单：
        // 排掉 hls/lls 边缘与纯音频/签名 ws 地址，其余都当 flv 候选。
        if url.contains("only_audio=1")
            || url.contains("wsSecret")
            || host.contains("hls")
            || host.contains("lls")
        {
            continue;
        }
        let stream_id = cap
            .get(1)
            .map(|g| g.as_str().to_string())
            .unwrap_or_default();
        cands.push(Cand {
            url,
            stream_id,
            order: idx,
        });
    }
    if cands.is_empty() {
        return Vec::new();
    }
    let mut ids: Vec<String> = cands.iter().map(|c| c.stream_id.clone()).collect();
    {
        let mut seen: Vec<String> = Vec::new();
        for id in ids.drain(..) {
            if !seen.contains(&id) {
                seen.push(id);
            }
        }
        ids = seen;
    }
    struct Group {
        id: String,
        first_pos: usize,
        width: u32,
        height: u32,
        urls: Vec<String>,
        best_rank: i32,
    }
    let mut groups: Vec<Group> = Vec::new();
    for id in ids.iter().cloned().collect::<Vec<String>>() {
        let Some(first_pos) = html.find(&format!("stream-{}", id)) else {
            continue;
        };
        // 分辨率窗口只到下一个「不同 stream id」出现的位置为止：两套源的块在页面里
        // 是紧挨着的，跨过去会把别人的画布读成本源的。
        let block_end = ids
            .iter()
            .filter(|other| other.as_str() != id)
            .filter_map(|other| {
                html[first_pos..]
                    .find(&format!("stream-{}", other))
                    .map(|offset| first_pos + offset)
            })
            .min()
            .unwrap_or(html.len());
        let end = (first_pos + ORIGIN_PAGE_WINDOW).min(block_end);
        let window = &html[first_pos..end];
        let mut max_pixels = 0u64;
        let mut width = 0u32;
        let mut height = 0u32;
        for cap in res_re.captures_iter(window) {
            let w: u32 = cap[1].parse().unwrap_or(0);
            let h: u32 = cap[2].parse().unwrap_or(0);
            if (w as u64) * (h as u64) > max_pixels {
                max_pixels = (w as u64) * (h as u64);
                width = w;
                height = h;
            }
        }
        let mut group: Vec<&Cand> = cands.iter().filter(|c| c.stream_id == id).collect();
        group.sort_by(|a, b| {
            douyin_template_rank(&douyin_template_of(&b.url))
                .cmp(&douyin_template_rank(&douyin_template_of(&a.url)))
                .then(a.order.cmp(&b.order))
        });
        let urls: Vec<String> = group.iter().map(|c| c.url.clone()).collect();
        let best_rank = urls
            .first()
            .map(|u| douyin_template_rank(&douyin_template_of(u)))
            .unwrap_or(i32::MIN);
        groups.push(Group {
            id,
            first_pos,
            width,
            height,
            urls,
            best_rank,
        });
    }
    // 排序四层：① 横画布且有 hd 及以上模板（游戏全屏 + 摄像头 PIP 的高清源）；
    // ② 有高清模板的其它源（竖屏合成源 1080p：布局是上下堆叠，但比 360p 的横屏
    // 源清楚得多 —— 现场房间 511309230149 的横屏源只给 _md 360p，旧规则让原画
    // 比低档还糊）；③ 只有低清模板的横屏源；④ 其余。同层内取最接近 16:9 的
    // （合成源 1920x1810 的 |9w-16h| 远大于游戏源 1920x1080 的 0）。
    groups.sort_by(|a, b| {
        let landscape = |g: &Group| g.width > g.height && g.width > 0;
        let tier = |g: &Group| match (landscape(g), g.best_rank >= DOUYIN_HIGH_DEF_RANK) {
            (true, true) => 0u8,
            (false, true) => 1,
            (true, false) => 2,
            (false, false) => 3,
        };
        let skew = |g: &Group| {
            let lhs = 9u64 * g.width as u64;
            let rhs = 16u64 * g.height as u64;
            lhs.abs_diff(rhs)
        };
        tier(a)
            .cmp(&tier(b))
            .then(skew(a).cmp(&skew(b)))
            .then(a.first_pos.cmp(&b.first_pos))
    });
    let mut out = Vec::new();
    for group in groups {
        println!(
            "[Douyin Stream Detail] page scrape id={} canvas={}x{} landscape={} best_tpl={} candidates={}",
            group.id,
            group.width,
            group.height,
            group.width > group.height && group.width > 0,
            group
                .urls
                .first()
                .map(|u| douyin_template_of(u))
                .unwrap_or_default(),
            group.urls.len()
        );
        for url in group.urls {
            if !out.contains(&url) {
                out.push(url);
            }
        }
    }
    out
}

async fn pick_live_origin_flv(http_client: &HttpClient, candidates: Vec<String>) -> Option<String> {
    const ORIGIN_PROBE_LIMIT: usize = 3;
    const ORIGIN_PROBE_BYTES: usize = 48 * 1024;
    const ORIGIN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    for url in candidates.iter().take(ORIGIN_PROBE_LIMIT) {
        let tpl = douyin_template_of(url);
        let probe = async {
            let mut resp = http_client.get_with_cookies(url).await?;
            let mut buf: Vec<u8> = Vec::new();
            while buf.len() < ORIGIN_PROBE_BYTES {
                match resp.chunk().await {
                    Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
                    Ok(None) => break,
                    Err(err) => return Err::<(), String>(err.to_string()),
                }
            }
            if buf.len() >= 3 && &buf[0..3] == b"FLV" {
                Ok(())
            } else {
                Err(format!("not an FLV body, got {} bytes", buf.len()))
            }
        };
        match tokio::time::timeout(ORIGIN_PROBE_TIMEOUT, probe).await {
            Ok(Ok(())) => {
                println!(
                    "[Douyin Stream Detail] origin template '{}' verified, using it",
                    tpl
                );
                return Some(url.clone());
            }
            Ok(Err(err)) => println!(
                "[Douyin Stream Detail] origin template '{}' rejected: {}",
                tpl, err
            ),
            Err(_) => println!(
                "[Douyin Stream Detail] origin template '{}' probe timed out",
                tpl
            ),
        }
    }
    println!(
        "[Douyin Stream Detail] no origin template verified, falling back to first candidate (template '{}')",
        douyin_template_of(candidates.first()?)
    );
    candidates.first().cloned()
}

#[cfg(test)]
mod douyin_page_extract_tests {
    use super::{
        douyin_template_of, douyin_template_rank, extract_origin_flv_candidates_from_html,
        pick_live_origin_flv,
    };
    use crate::platforms::common::http_client::HttpClient;

    /// 复刻官方页面结构：每块先是 flv_pull_url（各画质模板），紧随 sdk_params 的
    /// resolution；两套源的块在页面里紧挨着。横画布那套（游戏全屏 + 摄像头 PIP）
    /// 必须排在竖屏合成源之前，且模板按画质降序（uhd 在 hd 前）。
    fn two_source_page() -> String {
        let game = concat!(
            r#""flv_pull_url":{"ORIGIN":"https://pull-flv-q13.douyincdn.com/thirdgame/stream-111_Thirdgame0T010uhd.flv?expire=1","FULL_HD1":"https://pull-flv-q13.douyincdn.com/thirdgame/stream-111_Thirdgame0T010hd.flv?expire=1"},"#,
            r#""sdk_params":"{\"resolution\":\"1920x1080\"}"#
        );
        let composite = concat!(
            r#""flv_pull_url":{"ORIGIN":"https://pull-flv-q11.douyincdn.com/live/stream-222_uhd.flv?expire=1","HD1":"https://pull-flv-q11.douyincdn.com/live/stream-222_hd.flv?expire=1"},"#,
            r#""sdk_params":"{\"resolution\":\"1920x1810\"}"#
        );
        format!(
            r#"<script>{}{}{}</script>"#,
            "window.__a=", game, ";window.__b="
        ) + composite
    }

    #[test]
    fn prefers_landscape_game_source_at_best_template() {
        let cands = extract_origin_flv_candidates_from_html(&two_source_page());
        assert!(!cands.is_empty(), "no candidate extracted");
        assert!(
            cands[0].contains("stream-111_Thirdgame0T010uhd.flv"),
            "expected the landscape uhd template first, got '{}'",
            douyin_template_of(&cands[0])
        );
        assert!(
            cands[1].contains("stream-111_Thirdgame0T010hd.flv"),
            "hd of the same source should follow uhd, got '{}'",
            douyin_template_of(&cands[1])
        );
        let composite_pos = cands
            .iter()
            .position(|u| u.contains("stream-222"))
            .expect("portrait composite should still be a fallback");
        assert!(
            composite_pos >= 2,
            "composite must not be first, pos={}",
            composite_pos
        );
    }

    /// 横屏游戏源只给低清模板时（现场房间 511309230149：横屏源只有 _md 360p，
    /// 合成源有 _uhd 1080p），原画必须回退合成源的高清，而不是拿 360p 横屏。
    #[test]
    fn lowres_landscape_source_loses_to_highres_composite() {
        let page = concat!(
            r#"<script>"flv_pull_url":{"ORIGIN":"https://pull-flv-q13.douyincdn.com/thirdgame/stream-111_md.flv?expire=1"},"#,
            r#""sdk_params":"{"resolution":"1920x1080"}"#,
            r#";"flv_pull_url":{"ORIGIN":"https://pull-flv-q11.douyincdn.com/live/stream-222_uhd.flv?expire=1"},"#,
            r#""sdk_params":"{"resolution":"1920x1810"}"</script>"#
        );
        let cands = extract_origin_flv_candidates_from_html(page);
        assert!(
            cands[0].contains("stream-222_uhd.flv"),
            "composite 1080p must beat landscape 360p, got '{}'",
            cands[0]
        );
        assert!(cands[1].contains("stream-111_md.flv"), "game source stays as fallback");
    }

    /// 只有竖屏源（普通秀场房间）时不能一个都不返回，否则原画档位会整体失效。
    #[test]
    fn portrait_only_room_still_yields_candidates() {
        let page = r#"<script>"flv_pull_url":{"ORIGIN":"https://pull-flv-q11.douyincdn.com/live/stream-333_uhd.flv?expire=1"},"sdk_params":"{\"resolution\":\"1080x1920\"}"</script>"#;
        let cands = extract_origin_flv_candidates_from_html(page);
        assert_eq!(cands.len(), 1, "expected the single portrait url");
        assert!(cands[0].contains("stream-333_uhd.flv"), "got {}", cands[0]);
    }

    /// 页面内嵌 JSON 里 URL 以 \" 收尾，正则会连带吃掉结尾的裸反斜杠；
    /// 混进拉流地址会让播放器拿到非法 URL（现场房间 932584152016 实测复现）。
    #[test]
    fn escaped_json_url_loses_trailing_backslash() {
        let page = r#"<script>"flv_pull_url":{"ORIGIN":"https://pull-q5.douyincdn.com/thirdgame/stream-444_uhd.flv?expire=1\u0026sign=abc123\"}</script>"#;
        let cands = extract_origin_flv_candidates_from_html(page);
        assert_eq!(cands.len(), 1, "expected the single escaped url");
        assert!(
            !cands[0].ends_with('\\'),
            "trailing backslash leaked into url: '{}'",
            cands[0]
        );
        assert!(
            cands[0].ends_with("sign=abc123"),
            "query part lost or polluted: '{}'",
            cands[0]
        );
    }

    /// 在播房间实测：打印每个房间页面暴露的 stream id、画布与首选模板，
    /// 用来确认「横画布优先 + 模板降序」在多机位游戏房间上真的选对了。
    /// 房间列表接口要有效 msToken（DTV_TEST_MSTOKEN），拿不到就跳过而不是失败。
    #[tokio::test]
    #[ignore = "needs network + DTV_TEST_MSTOKEN"]
    async fn live_douyin_page_layout_probe() {
        use crate::platforms::douyin::douyin_streamer_list::fetch_douyin_partition_rooms_core;
        let list = match fetch_douyin_partition_rooms_core(
            String::new(),
            String::new(),
            0,
            std::env::var("DTV_TEST_MSTOKEN").unwrap_or_default(),
        )
        .await
        {
            Ok(list) => list,
            Err(err) => {
                println!("[layout probe] skip, 拿不到在播房间列表, err: {}", err);
                return;
            }
        };
        let client = HttpClient::new_direct_connection().expect("client");
        for room in list.rooms.iter().take(12) {
            let url = format!("https://live.douyin.com/{}", room.web_rid);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_static(
                    crate::platforms::douyin::web_api::DEFAULT_USER_AGENT,
                ),
            );
            headers.insert(
                reqwest::header::REFERER,
                reqwest::header::HeaderValue::from_str(&url).expect("referer"),
            );
            let html = match client.get_text_with_headers(&url, Some(headers)).await {
                Ok(html) => html,
                Err(err) => {
                    println!("[layout probe] {} page failed: {}", room.web_rid, err);
                    continue;
                }
            };
            let cands = extract_origin_flv_candidates_from_html(&html);
            let id_re = regex::Regex::new(r"stream-(\d+)").expect("id regex");
            let ids: Vec<String> = {
                let mut v: Vec<String> = cands
                    .iter()
                    .filter_map(|u| id_re.captures(u).map(|c| c[1].to_string()))
                    .collect();
                v.sort();
                v.dedup();
                v
            };
            println!(
                "[layout probe] {} srcs={} cands={} chosen_tpl={} title={}",
                room.web_rid,
                ids.len(),
                cands.len(),
                cands
                    .first()
                    .map(|u| douyin_template_of(u))
                    .unwrap_or_default(),
                room.title.chars().take(18).collect::<String>()
            );
        }
    }

    /// 需要实抓的 live.douyin.com 页面（多机位游戏房间）才有意义，平时忽略。
    #[test]
    #[ignore = "needs a captured /tmp/dy_page.html from a multi-source douyin room"]
    fn real_page_prefers_landscape_game_source() {
        let html = std::fs::read_to_string("/tmp/dy_page.html").expect("fixture");
        let cands = extract_origin_flv_candidates_from_html(&html);
        assert!(!cands.is_empty(), "no candidate extracted");
        assert!(
            cands[0].contains("408592726494544702"),
            "expected landscape game stream 494544702 first, got {}",
            cands[0]
        );
        assert!(
            douyin_template_of(&cands[0]).to_lowercase().contains("uhd"),
            "expected the uhd template first (measured 1920x1080 h264), got '{}'",
            douyin_template_of(&cands[0])
        );
    }

    /// 抖音 CDN 对不可用模板回 302（本次实测 hd/ld/or4 都 302，只有 uhd 与 md 出流）。
    /// 选原画地址时必须跳过 302 的模板，落到下一个能出 FLV 的。
    #[tokio::test]
    async fn skips_templates_that_do_not_serve_flv() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if req.contains("/dead/") {
                    let empty = "HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n";
                    let _ = sock.write_all(empty.as_bytes()).await;
                    continue;
                }
                let mut body = Vec::new();
                body.extend_from_slice(b"FLV");
                body.extend_from_slice(&[1u8, 0, 0, 0, 9]);
                body.resize(64 * 1024, 0);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        let client = HttpClient::new_direct_connection().expect("client");
        let candidates = vec![
            format!(
                "http://127.0.0.1:{}/dead/stream-9_Thirdgame0T010uhd.flv",
                port
            ),
            format!(
                "http://127.0.0.1:{}/live/stream-9_Thirdgame0T010hd.flv",
                port
            ),
        ];
        let picked = pick_live_origin_flv(&client, candidates)
            .await
            .expect("must fall back to a live template");
        assert!(
            picked.contains("/live/"),
            "must skip the 302 template, got {}",
            picked
        );
    }

    #[test]
    fn template_rank_matches_measured_bitrates() {
        let rank = |tpl: &str| douyin_template_rank(tpl);
        assert!(rank("Thirdgame0T010uhd") > rank(""), "uhd above origin");
        assert!(rank("") > rank("Thirdgame0T010hd"), "origin above hd");
        assert!(rank("Thirdgame0T010hd") > rank("sd"), "hd above sd");
        assert!(rank("sd") > rank("ld"), "sd above ld");
        assert!(rank("ld") > rank("md"), "ld above md (md measured 640x360)");
        assert!(
            rank("sd") > rank("lssd5"),
            "low-latency variants rank below normal"
        );
        assert_eq!(
            douyin_template_of(
                "https://h.douyincdn.com/x/stream-408592726494544702_Thirdgame0T010uhd.flv?a=1"
            ),
            "Thirdgame0T010uhd"
        );
    }
}

fn unescape_js_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some('u') => {
                    chars.next();
                    let mut hex = String::new();
                    for _ in 0..4 {
                        if let Some(h) = chars.next() {
                            hex.push(h);
                        } else {
                            break;
                        }
                    }
                    if let Ok(code) = u16::from_str_radix(&hex, 16) {
                        if let Some(decoded) = char::from_u32(code as u32) {
                            out.push(decoded);
                            continue;
                        }
                    }
                    out.push('\\');
                    out.push('u');
                    out.push_str(&hex);
                }
                Some('/') => {
                    chars.next();
                    out.push('/');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                Some('"') => {
                    chars.next();
                    out.push('"');
                }
                Some('\'') => {
                    chars.next();
                    out.push('\'');
                }
                Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('r') => {
                    chars.next();
                    out.push('\r');
                }
                Some('t') => {
                    chars.next();
                    out.push('\t');
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}
