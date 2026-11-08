use ::cookie::Cookie;
use reqwest::header::HeaderValue;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};
use tauri::{AppHandle, Manager};
use url::Url;

struct Spec {
    webview_url: &'static str,
    domain: &'static str,
    /// 命中任意一个即认定已登录：这些名字只有登录成功后服务端才下发。
    login_names: &'static [&'static str],
}

const DOUYU_SPEC: Spec = Spec {
    webview_url: "https://www.douyu.com/",
    domain: "douyu.com",
    login_names: &["acf_uid", "acf_stk", "acf_nickname"],
};

const HUYA_SPEC: Spec = Spec {
    webview_url: "https://www.huya.com/",
    domain: "huya.com",
    login_names: &["huya_nickname", "yyb", "_mk"],
};

fn normalize(platform: &str) -> Option<String> {
    let trimmed = platform.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

fn spec(platform: &str) -> Option<&'static Spec> {
    match normalize(platform)?.as_str() {
        "douyu" => Some(&DOUYU_SPEC),
        "huya" => Some(&HUYA_SPEC),
        _ => None,
    }
}

fn registry() -> &'static RwLock<HashMap<String, String>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 解析出的 cookie 存进进程级注册表：流地址解析发生在 Rust 内部（含代理续签、
/// flv relay），拿不到前端参数，所以按平台名从这里读。
pub fn set(platform: &str, cookie: Option<String>) {
    let name = match normalize(platform) {
        Some(name) => name,
        None => {
            crate::tl!("web", "[login] skip, 平台名为空, 不写入登录 cookie");
            return;
        }
    };
    if spec(&name).is_none() {
        crate::tl!(
            "web",
            "[login] skip, 该平台没有登录 cookie 支持, platform: {}",
            name
        );
        return;
    }
    let trimmed = cookie
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty());
    let logged_in = is_cookie_logged_in(spec(&name), trimmed.as_deref());
    let mut map = registry()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match trimmed {
        Some(value) => {
            let bytes = value.len();
            let replaced = map.insert(name.clone(), value).is_some();
            crate::tl!(
                "web",
                "[login] set_platform_cookie platform={} loggedIn={} cookieBytes={} replaced={}",
                name,
                logged_in,
                bytes,
                replaced
            );
        }
        None => {
            let had_value = map.remove(&name).is_some();
            crate::tl!(
                "web",
                "[login] clear_platform_cookie platform={} hadValue={}",
                name,
                had_value
            );
        }
    }
}

pub fn value(platform: &str) -> Option<String> {
    let name = normalize(platform)?;
    registry()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&name)
        .cloned()
}

pub fn is_logged_in(platform: &str) -> bool {
    let name = normalize(platform).unwrap_or_default();
    is_cookie_logged_in(spec(&name), value(&name).as_deref())
}

pub fn flags(platform: &str) -> BTreeMap<String, bool> {
    let name = normalize(platform).unwrap_or_default();
    cookie_flags(spec(&name), value(&name).as_deref())
}

/// 请求头只接受 ASCII 可见字符：昵称、头像这类 cookie 可能是原始 UTF-8，
/// 塞进 HeaderValue 会构造失败，所以非 ASCII 的对直接丢掉（认证 cookie 都是 ASCII）。
pub fn header(platform: &str) -> Option<HeaderValue> {
    let raw = value(platform)?;
    let pairs = cookie_pairs(&raw);
    let kept = pairs
        .iter()
        .filter(|(name, value)| is_header_safe(name) && is_header_safe(value))
        .map(|(name, value)| format!("{}={}", name, value))
        .collect::<Vec<String>>()
        .join("; ");
    if kept.is_empty() {
        crate::tl!(
            "web",
            "[login] skip, 登录 cookie 值全非 ASCII, 不进请求头, platform: {}, pairs: {}",
            platform,
            pairs.len()
        );
        return None;
    }
    match HeaderValue::from_str(&kept) {
        Ok(value) => Some(value),
        Err(err) => {
            crate::tl!(
                "web",
                "[login] fail, 登录 cookie 构造请求头失败, platform: {}, err: {}",
                platform,
                err
            );
            None
        }
    }
}

fn is_header_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

fn cookie_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .map(|pair| pair.trim())
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn cookie_flags(spec: Option<&'static Spec>, raw: Option<&str>) -> BTreeMap<String, bool> {
    let Some(spec) = spec else {
        return BTreeMap::new();
    };
    let pairs = cookie_pairs(raw.unwrap_or(""));
    spec.login_names
        .iter()
        .map(|name| {
            let present = pairs.iter().any(|(key, value)| {
                key.eq_ignore_ascii_case(name) && !value.is_empty() && value != "0"
            });
            ((*name).to_string(), present)
        })
        .collect()
}

fn is_cookie_logged_in(spec: Option<&'static Spec>, raw: Option<&str>) -> bool {
    cookie_flags(spec, raw).values().any(|flag| *flag)
}

fn keep_domain_cookies(spec: &Spec, cookies: Vec<Cookie<'static>>, into: &mut BTreeMap<String, String>) {
    for cookie in cookies {
        let domain_matches = cookie
            .domain()
            .map(|domain| domain.contains(spec.domain))
            .unwrap_or(false);
        if !domain_matches {
            continue;
        }
        into
            .entry(cookie.name().to_string())
            .or_insert_with(|| cookie.value().to_string());
    }
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlatformCookieResult {
    pub platform: String,
    pub cookie: Option<String>,
    pub flags: BTreeMap<String, bool>,
    pub has_required: bool,
}

impl PlatformCookieResult {
    fn empty(platform: &str) -> Self {
        Self {
            platform: platform.to_string(),
            cookie: None,
            flags: cookie_flags(spec(platform), None),
            has_required: false,
        }
    }
}

fn spec_or_err(platform: &str) -> Result<&'static Spec, String> {
    spec(platform).ok_or_else(|| {
        crate::tl!(
            "web",
            "[login] fail, 该平台没有登录 cookie 支持, platform: {}",
            platform
        );
        "PLATFORM_LOGIN_UNSUPPORTED".to_string()
    })
}

/// 从登录窗口里把 cookie 捞出来（含 httpOnly），交给前端存 localStorage。
#[tauri::command]
pub async fn get_platform_cookie(
    app_handle: AppHandle,
    platform: String,
    labels: Option<Vec<String>>,
    url: Option<String>,
) -> Result<PlatformCookieResult, String> {
    let spec = spec_or_err(&platform)?;
    let target = url
        .filter(|raw| !raw.trim().is_empty())
        .unwrap_or_else(|| spec.webview_url.to_string());
    let parsed_url = Url::parse(&target).map_err(|e| format!("Invalid URL: {}", e))?;

    let mut labels = labels.unwrap_or_default();
    if labels.is_empty() {
        labels = app_handle.webview_windows().keys().cloned().collect();
    }
    labels.sort();
    labels.dedup();

    let mut collected: BTreeMap<String, String> = BTreeMap::new();
    for label in &labels {
        let Some(window) = app_handle.get_webview_window(label) else {
            continue;
        };
        if let Ok(cookies) = window.cookies_for_url(parsed_url.clone()) {
            keep_domain_cookies(spec, cookies, &mut collected);
        }
        if let Ok(cookies) = window.cookies() {
            keep_domain_cookies(spec, cookies, &mut collected);
        }
    }

    let platform_name = normalize(&platform).unwrap_or_else(|| platform.clone());
    if collected.is_empty() {
        crate::tl!(
            "web",
            "[login] miss, 登录窗口未取到该域名 cookie, platform: {}, labels: {:?}",
            platform_name,
            labels
        );
        return Ok(PlatformCookieResult::empty(&platform_name));
    }

    let cookie = collected
        .iter()
        .map(|(name, value)| format!("{}={}", name, value))
        .collect::<Vec<String>>()
        .join("; ");
    let flags = cookie_flags(Some(spec), Some(&cookie));
    let has_required = flags.values().any(|flag| *flag);
    crate::tl!(
        "web",
        "[login] hit, 登录窗口取到 cookie, platform: {}, count: {}, hasRequired: {}",
        platform_name,
        collected.len(),
        has_required
    );
    Ok(PlatformCookieResult {
        platform: platform_name,
        cookie: Some(cookie),
        flags,
        has_required,
    })
}

/// 前端对非 main 窗口没有 core:window 权限（capabilities 的 windows 只列了 main），
/// 登录窗口的关闭只能走 Rust 侧，否则扫码完成后窗口会一直挂着。
#[tauri::command]
pub fn close_webview_window_by_label(app_handle: AppHandle, label: String) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        crate::tl!("web", "[login] skip, 窗口 label 为空不关闭");
        return false;
    }
    let Some(window) = app_handle.get_webview_window(trimmed) else {
        crate::tl!("web", "[login] skip, 窗口已不存在无需关闭, label: {}", trimmed);
        return false;
    };
    match window.close() {
        Ok(()) => {
            crate::tl!("web", "[login] done, 已关闭登录窗口, label: {}", trimmed);
            true
        }
        Err(err) => {
            crate::tl!(
                "web",
                "[login] fail, 关闭登录窗口失败, label: {}, err: {}",
                trimmed,
                err
            );
            false
        }
    }
}

/// 前端把 localStorage 里的 cookie 推进注册表：Rust 侧解析流地址时直接读，
/// 同时覆盖代理续签与 relay 这些拿不到前端参数的内部路径。
#[tauri::command]
pub fn set_platform_cookie(platform: String, cookie: Option<String>) -> Result<PlatformCookieResult, String> {
    spec_or_err(&platform)?;
    set(&platform, cookie);
    let platform_name = normalize(&platform).unwrap_or_else(|| platform.clone());
    Ok(PlatformCookieResult {
        platform: platform_name.clone(),
        cookie: value(&platform_name),
        flags: flags(&platform_name),
        has_required: is_logged_in(&platform_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_cookie_survives_and_utf8_value_is_dropped() {
        set("douyu", Some("acf_uid=123; acf_nickname=主播; PHPSESSID=abc".to_string()));
        let header = header("douyu").expect("ascii pairs should remain");
        let text = header.to_str().unwrap().to_string();
        assert!(text.contains("acf_uid=123"), "text: {}", text);
        assert!(text.contains("PHPSESSID=abc"), "text: {}", text);
        assert!(!text.contains("acf_nickname"), "text: {}", text);
        assert!(is_logged_in("douyu"));
        assert_eq!(flags("douyu").get("acf_uid"), Some(&true));
        set("douyu", None);
        assert_eq!(value("douyu"), None);
        assert!(!is_logged_in("douyu"));
    }

    #[test]
    fn unknown_platform_is_not_supported() {
        assert!(spec("douyin").is_none());
        set("douyin", Some("a=b".to_string()));
        assert_eq!(value("douyin"), None);
        assert!(!is_logged_in("douyin"));
    }

    #[test]
    fn guest_cookie_without_login_names_is_not_logged_in() {
        set("huya", Some("PHPSESSID=guest-session".to_string()));
        assert_eq!(value("huya").as_deref(), Some("PHPSESSID=guest-session"));
        assert!(!is_logged_in("huya"));
        set("huya", Some("huya_nickname=%E4%B8%BB%E6%92%AD; yyb=9527".to_string()));
        assert!(is_logged_in("huya"));
        let header = header("huya").expect("percent-encoded value is ascii");
        assert!(header.to_str().unwrap().contains("huya_nickname=%E4%B8%BB%E6%92%AD"));
        set("huya", None);
    }
}
