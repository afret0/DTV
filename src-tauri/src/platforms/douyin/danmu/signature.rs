use log::{debug, error};
use md5::{Digest, Md5};
use rand::Rng;
use std::collections::HashMap;
#[cfg(not(target_os = "android"))]
use deno_core::{FastString, JsRuntime, RuntimeOptions};
#[cfg(target_os = "linux")]
use std::sync::Once;
use url::Url;

// Load sign.js content at compile time
const SIGN_JS_CONTENT: &str = include_str!("./sign.js");

#[cfg(target_os = "linux")]
static JS_RUNTIME_INIT: Once = Once::new();

#[cfg(not(target_os = "android"))]
fn ensure_js_runtime_platform_initialized() {
    #[cfg(target_os = "linux")]
    JS_RUNTIME_INIT.call_once(|| {
        JsRuntime::init_platform(None);
    });
}

/// Build the string that must be signed for the given danmaku WSS url and
/// return its MD5 hex digest (shared by the V8 and QuickJS backends).
fn md5_param_for_wss_url(wss_url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed_url = Url::parse(wss_url)?;
    let params_to_sign_keys = [
        "live_id",
        "aid",
        "version_code",
        "webcast_sdk_version",
        "room_id",
        "sub_room_id",
        "sub_channel_id",
        "did_rule",
        "user_unique_id",
        "device_platform",
        "device_type",
        "ac",
        "identity",
    ];
    let mut query_params_map = HashMap::new();
    for (key, value) in parsed_url.query_pairs() {
        query_params_map.insert(key.into_owned(), value.into_owned());
    }

    let mut tpl_params_vec: Vec<String> = Vec::new();
    for key_str in params_to_sign_keys {
        let value = query_params_map
            .get(key_str)
            .map(|s| s.as_str())
            .unwrap_or("");
        tpl_params_vec.push(format!("{}={}", key_str, value));
    }
    let to_sign_str = tpl_params_vec.join(",");
    debug!("[Douyin Danmaku] String to MD5 for signature: {}", to_sign_str);

    let mut hasher = Md5::new();
    hasher.update(to_sign_str.as_bytes());
    let digest_bytes = hasher.finalize();
    let md5_param = format!("{:x}", digest_bytes);
    debug!("[Douyin Danmaku] MD5 param for signature: {}", md5_param);
    Ok(md5_param)
}

const SIGN_BOOTSTRAP: &str = r#"
        globalThis.window = globalThis;
        globalThis.self = globalThis;
        globalThis.document = {};
        globalThis.navigator = {
            userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        };
    "#;

/// QuickJS backend (Android): V8 cannot run inside Android app processes
/// (W^X). Executing deno_core there crashes the whole app with SIGBUS on a
/// tokio worker, so the danmaku signature is computed with QuickJS instead.
pub fn quickjs_generate_signature(md5_param: &str) -> Result<String, String> {
    let rt = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
    let ctx = rquickjs::Context::full(&rt).map_err(|e| e.to_string())?;
    ctx.with(|ctx| -> Result<String, String> {
        let _: () = ctx.eval(SIGN_BOOTSTRAP).map_err(|e| e.to_string())?;
        let _: () = ctx.eval(SIGN_JS_CONTENT).map_err(|e| e.to_string())?;
        let call_script = format!("get_sign('{}')", md5_param);
        let v: String = ctx.eval(call_script.as_str()).map_err(|e| e.to_string())?;
        Ok(v)
    })
}

#[cfg(target_os = "android")]
pub async fn generate_signature(
    wss_url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let md5_param = md5_param_for_wss_url(wss_url)?;
    let out = tokio::task::spawn_blocking(move || quickjs_generate_signature(&md5_param))
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
        })?
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            error!("[Douyin Danmaku] quickjs signature error: {}", e);
            Box::new(std::io::Error::new(std::io::ErrorKind::Other, e))
        })?;
    debug!("[Douyin Danmaku] Final signature computed (quickjs).");
    Ok(out)
}

#[cfg(not(target_os = "android"))]
pub async fn generate_signature(
    wss_url: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let md5_param = md5_param_for_wss_url(wss_url)?;

    ensure_js_runtime_platform_initialized();
    let mut runtime = JsRuntime::new(RuntimeOptions::default());

    runtime
        .execute_script("[bootstrap]", FastString::from_static(SIGN_BOOTSTRAP))
        .map_err(|e| {
            error!("[Douyin Danmaku] Error during deno_core bootstrap script: {}", e);
            e
        })?;

    // Use the embedded sign.js content
    runtime
        .execute_script("./sign.js", FastString::from_static(SIGN_JS_CONTENT))
        .map_err(|e| {
            error!("[Douyin Danmaku] Error during deno_core eval of sign.js: {}", e);
            e
        })?;

    let call_script = format!("get_sign('{}')", md5_param);
    // For dynamic strings, convert to String first, then into FastString
    let fast_call_script = FastString::from(call_script);
    let result = runtime
        .execute_script("[call_get_sign]", fast_call_script)
        .map_err(|e| {
            error!("[Douyin Danmaku] Error during deno_core call to get_sign: {}", e);
            e
        })?;

    let scope = &mut runtime.handle_scope();
    let local_value = deno_core::v8::Local::new(scope, result);

    if local_value.is_string() {
        let signature = local_value.to_rust_string_lossy(scope);
        debug!("[Douyin Danmaku] Final signature computed.");
        Ok(signature)
    } else {
        Err(
            Box::from("get_sign did not return a string value from deno_core")
                as Box<dyn std::error::Error + Send + Sync>,
        )
    }
}

pub fn generate_ms_token(length: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"; // Removed _= as they are not typical for msToken
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[tauri::command]
pub fn generate_douyin_ms_token() -> String {
    // For now, let's assume msToken length is always 107, as used elsewhere.
    // If variable length is needed, this command could take a length parameter.
    generate_ms_token(107)
}

// Placeholder for the more complex signature generation if needed later.
// pub async fn generate_signature(wss_url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
//     // ... (implementation from demo if required)
//     unimplemented!();
// }

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;

    const SAMPLE_URL: &str = "wss://webcast3-ws-web-lq.douyin.com/webcast/im/push/v2/?app_name=douyin_web&version_code=180800&webcast_sdk_version=1.0.14-beta.0&update_version_code=1.0.14-beta.0&compress=gzip&device_platform=web&cookie_enabled=true&screen_width=1920&screen_height=1080&browser_language=zh-CN&browser_platform=MacIntel&browser_name=Mozilla&browser_version=5.0&browser_online=true&tz_name=Asia%2FShanghai&cursor=t-1727700000000_r-1_d-1_u-1&last_rtt=0&fetch_rule=1&resp_content_type=protobuf&identity=audience&room_id=7419149258225388324&heartbeat_duration=10000&live_id=1&did_rule=3&aid=6383&sub_room_id=0&sub_channel_id=0&user_unique_id=7419000000000000000&device_type=&ac=wifi";

    /// The Android QuickJS backend must produce byte-identical signatures to
    /// the desktop deno_core/V8 backend, otherwise danmaku auth would fail.
    #[tokio::test]
    async fn quickjs_matches_deno_signature() {
        let md5 = md5_param_for_wss_url(SAMPLE_URL).expect("md5");
        let deno_sig = generate_signature(SAMPLE_URL).await.expect("deno signature");
        let deno_sig2 = generate_signature(SAMPLE_URL).await.expect("deno signature 2");
        println!("deno  #1: {}", deno_sig);
        println!("deno  #2: {}", deno_sig2);
        let md5b = md5.clone();
        let qjs_sig =
            tokio::task::spawn_blocking(move || quickjs_generate_signature(&md5b))
                .await
                .expect("spawn")
                .expect("quickjs signature");
        println!("quickjs : {}", qjs_sig);
        assert!(!deno_sig.is_empty());
        if deno_sig == deno_sig2 {
            // deterministic signer: engines must agree byte-for-byte
            assert_eq!(deno_sig, qjs_sig, "quickjs signature must match deno/V8");
        } else {
            // time/random dependent signer: compare structure only
            assert_eq!(deno_sig.len(), qjs_sig.len(), "signature length parity");
        }
    }
}
