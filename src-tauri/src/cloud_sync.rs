//! 配置云同步：把 LAN sync 的同一份 payload JSON 存到阿里云 OSS 或任意 WebDAV。
//! 放在 Rust 侧是因为 WKWebView 的 fetch 带 tauri:// Origin，OSS/WebDAV 服务端
//! 基本都不回 CORS 头，浏览器侧请求必失败；reqwest 没有这个限制。

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct OssSettings {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub access_key_secret: String,
    pub object_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct WebDavSettings {
    pub url: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudSyncSettings {
    /// "oss" | "webdav" | ""(未配置)
    pub provider: String,
    pub oss: OssSettings,
    pub webdav: WebDavSettings,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("cloud sync client")
}

fn oss_object_url(cfg: &OssSettings) -> Result<String, String> {
    let endpoint = cfg.endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() || cfg.bucket.trim().is_empty() || cfg.object_key.trim().is_empty() {
        return Err("OSS 配置不完整：endpoint / bucket / objectKey 都要填".into());
    }
    let endpoint = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };
    let key = cfg.object_key.trim().trim_start_matches('/');
    Ok(format!("{endpoint}/{key}"))
}

/// OSS V1 签名的签名体：base64(hmac-sha1(secret, stringToSign))。
fn oss_signature(secret: &str, verb: &str, date: &str, content_type: &str, resource: &str) -> String {
    let string_to_sign = format!("{verb}\n\n{content_type}\n{date}\n{resource}");
    let mut mac =
        Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// OSS V1 签名：Authorization: OSS <id>:<base64(hmac-sha1(secret, stringToSign))>
fn oss_authorization(cfg: &OssSettings, verb: &str, date: &str, content_type: &str) -> String {
    let key = cfg.object_key.trim().trim_start_matches('/');
    let resource = format!("/{}/{}", cfg.bucket.trim(), key);
    let sig = oss_signature(&cfg.access_key_secret, verb, date, content_type, &resource);
    format!("OSS {}:{}", cfg.access_key_id.trim(), sig)
}

fn rfc1123_now() -> String {
    chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

async fn oss_put(cfg: &OssSettings, body: String) -> Result<String, String> {
    let url = oss_object_url(cfg)?;
    let date = rfc1123_now();
    let auth = oss_authorization(cfg, "PUT", &date, "application/json");
    let resp = client()
        .put(&url)
        .header("Date", &date)
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("OSS 上传失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("OSS 上传返回 {}: {}", status.as_u16(), oss_err_msg(&text)));
    }
    Ok(format!("OSS 上传成功 ({})", status.as_u16()))
}

async fn oss_get(cfg: &OssSettings) -> Result<Option<String>, String> {
    let url = oss_object_url(cfg)?;
    let date = rfc1123_now();
    let auth = oss_authorization(cfg, "GET", &date, "");
    let resp = client()
        .get(&url)
        .header("Date", &date)
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| format!("OSS 下载失败: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("OSS 下载返回 {}: {}", status.as_u16(), oss_err_msg(&text)));
    }
    let text = resp.text().await.map_err(|e| format!("OSS 响应读取失败: {e}"))?;
    Ok(Some(text))
}

fn oss_err_msg(text: &str) -> String {
    // <Code>NoSuchKey</Code><Message>...</Message>
    if let (Some(a), Some(b)) = (text.find("<Message>"), text.find("</Message>")) {
        if b > a {
            return text[a + 9..b].to_string();
        }
    }
    text.chars().take(200).collect()
}

fn webdav_validate(cfg: &WebDavSettings) -> Result<String, String> {
    let url = cfg.url.trim();
    if url.is_empty() {
        return Err("WebDAV 地址不能为空".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("WebDAV 地址要以 http(s):// 开头".into());
    }
    Ok(url.to_string())
}

async fn webdav_put(cfg: &WebDavSettings, body: String) -> Result<String, String> {
    let url = webdav_validate(cfg)?;
    let mut req = client()
        .put(&url)
        .header("Content-Type", "application/json")
        .body(body);
    if !cfg.username.trim().is_empty() {
        req = req.basic_auth(cfg.username.trim(), Some(cfg.password.as_str()));
    }
    let resp = req.send().await.map_err(|e| format!("WebDAV 上传失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("WebDAV 上传返回 {}", status.as_u16()));
    }
    // 部分服务器对 PUT 新建返回 201，覆盖返回 204/200，都算成功
    Ok(format!("WebDAV 上传成功 ({})", status.as_u16()))
}

async fn webdav_get(cfg: &WebDavSettings) -> Result<Option<String>, String> {
    let url = webdav_validate(cfg)?;
    let mut req = client().get(&url);
    if !cfg.username.trim().is_empty() {
        req = req.basic_auth(cfg.username.trim(), Some(cfg.password.as_str()));
    }
    let resp = req.send().await.map_err(|e| format!("WebDAV 下载失败: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("WebDAV 下载返回 {}", status.as_u16()));
    }
    let text = resp.text().await.map_err(|e| format!("WebDAV 响应读取失败: {e}"))?;
    Ok(Some(text))
}

#[tauri::command]
pub async fn cloud_sync_push(settings: CloudSyncSettings, payload_json: String) -> Result<String, String> {
    serde_json::from_str::<serde_json::Value>(&payload_json)
        .map_err(|e| format!("payload 不是合法 JSON: {e}"))?;
    match settings.provider.as_str() {
        "oss" => oss_put(&settings.oss, payload_json).await,
        "webdav" => webdav_put(&settings.webdav, payload_json).await,
        _ => Err("未选择同步方式（OSS 或 WebDAV）".into()),
    }
}

#[tauri::command]
pub async fn cloud_sync_pull(settings: CloudSyncSettings) -> Result<Option<String>, String> {
    match settings.provider.as_str() {
        "oss" => oss_get(&settings.oss).await,
        "webdav" => webdav_get(&settings.webdav).await,
        _ => Err("未选择同步方式（OSS 或 WebDAV）".into()),
    }
}

/// 连通性自检：做一次 GET（对象不存在也算连通成功）。
#[tauri::command]
pub async fn cloud_sync_test(settings: CloudSyncSettings) -> Result<String, String> {
    match settings.provider.as_str() {
        "oss" => {
            oss_object_url(&settings.oss)?;
            match oss_get(&settings.oss).await {
                Ok(Some(t)) => Ok(format!("OSS 连接正常，远端已有配置 ({} 字节)", t.len())),
                Ok(None) => Ok("OSS 连接正常，远端还没有配置文件（首次点「上传」即可创建）".into()),
                Err(e) => Err(e),
            }
        }
        "webdav" => {
            webdav_validate(&settings.webdav)?;
            match webdav_get(&settings.webdav).await {
                Ok(Some(t)) => Ok(format!("WebDAV 连接正常，远端已有配置 ({} 字节)", t.len())),
                Ok(None) => Ok("WebDAV 连接正常，远端还没有配置文件（首次点「上传」即可创建）".into()),
                Err(e) => Err(e),
            }
        }
        _ => Err("未选择同步方式（OSS 或 WebDAV）".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与独立实现（python hmac-sha1/base64）对拍的已知向量。
    #[test]
    fn oss_signature_matches_known_vector() {
        let sig = oss_signature(
            "testsecret",
            "PUT",
            "Wed, 22 Oct 2026 12:00:00 GMT",
            "application/json",
            "/my-bucket/dtv/config-sync.json",
        );
        assert_eq!(sig, "56tsDW6+Vz1WXzt9DXHJ+r+elW0=");
    }

    #[test]
    fn oss_authorization_uses_bucket_and_key() {
        let cfg = OssSettings {
            endpoint: "https://oss-cn-hangzhou.aliyuncs.com".into(),
            bucket: "my-bucket".into(),
            access_key_id: "AKID".into(),
            access_key_secret: "testsecret".into(),
            object_key: "/dtv/config-sync.json".into(),
        };
        let auth = oss_authorization(&cfg, "PUT", "Wed, 22 Oct 2026 12:00:00 GMT", "application/json");
        assert_eq!(auth, "OSS AKID:56tsDW6+Vz1WXzt9DXHJ+r+elW0=");
    }

    #[test]
    fn oss_object_url_normalizes() {
        let cfg = OssSettings {
            endpoint: "oss-cn-hangzhou.aliyuncs.com/".into(),
            bucket: "b".into(),
            access_key_id: "i".into(),
            access_key_secret: "s".into(),
            object_key: "/k.json".into(),
        };
        assert_eq!(oss_object_url(&cfg).unwrap(), "https://oss-cn-hangzhou.aliyuncs.com/k.json");
        let bad = OssSettings { endpoint: "".into(), ..cfg.clone() };
        assert!(oss_object_url(&bad).is_err());
    }

    #[test]
    fn webdav_url_validation() {
        assert!(webdav_validate(&WebDavSettings { url: "https://d/x.json".into(), ..Default::default() }).is_ok());
        assert!(webdav_validate(&WebDavSettings { url: "ftp://d/x.json".into(), ..Default::default() }).is_err());
        assert!(webdav_validate(&WebDavSettings { url: "".into(), ..Default::default() }).is_err());
    }
}
