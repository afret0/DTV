import { invoke } from '@tauri-apps/api/core';

let douyuProxyActive = false;

export async function getDouyuStreamConfig(
  roomId: string,
  quality: string = '原画',
  line?: string | null,
): Promise<{ streamUrl: string, streamType: string | undefined }> {
  let finalStreamUrl: string | null = null;
  let streamType: string | undefined = undefined;
  const MAX_STREAM_FETCH_ATTEMPTS = 3;
  let offlineHits = 0;

  for (let attempt = 1; attempt <= MAX_STREAM_FETCH_ATTEMPTS; attempt++) {
    try {
      const streamUrl = await invoke<string>('get_stream_url_with_quality_cmd', {
        roomId: roomId,
        quality: quality,
        line: line ?? null,
      });
      
      if (streamUrl) {
        finalStreamUrl = enforceHttps(streamUrl);
        streamType = 'flv';
        break;
      } else {
        throw new Error('斗鱼直播流地址获取为空。');
      }
    } catch (e: any) {
      // Tauri rejects a failed command with the Rust Err(String) payload itself,
      // so `e` is usually a plain string -- `e.message` would be undefined and
      // the real cause (and offline detection) silently lost.
      const raw = typeof e === 'string' ? e : (e?.message ? String(e.message) : String(e ?? ''));
      console.error(`[DouyuPlayerHelper] 获取斗鱼直播流失败 (尝试 ${attempt}/${MAX_STREAM_FETCH_ATTEMPTS}):`, raw);

      const lower = raw.toLowerCase();
      // Exact code matches only: a loose "error: 1" prefix also matches codes
      // like 10/12/123 and mislabels transient API failures as offline.
      const exactOfflinePatterns = [/error:\s*1(?![0-9])/, /error:\s*102(?![0-9])/, /error\s+code\s*1(?![0-9])/, /error\s+code\s*102(?![0-9])/];
      const isDefinitivelyOffline =
        lower.includes('主播未开播') || lower.includes('房间不存在') ||
        exactOfflinePatterns.some((re) => re.test(lower));

      if (isDefinitivelyOffline) {
        offlineHits += 1;
        // betard/接口偶发误报未开播：再确认一次后才当真，避免把在线房间
        // 弹成“主播未开播”逼用户手动刷新。
        if (offlineHits >= 2 || attempt >= MAX_STREAM_FETCH_ATTEMPTS) {
          console.warn(`[DouyuPlayerHelper] Streamer for room ${roomId} confirmed offline (${offlineHits} hit(s)).`);
          throw new Error(raw.includes('主播未开播') || raw.includes('房间不存在') ? raw : '主播未开播');
        }
        await new Promise(resolve => setTimeout(resolve, 800));
        continue;
      }

      if (attempt >= MAX_STREAM_FETCH_ATTEMPTS) {
        throw new Error(`获取斗鱼直播流失败 (尝试 ${MAX_STREAM_FETCH_ATTEMPTS} 次后): ${raw}`);
      }
      await new Promise(resolve => setTimeout(resolve, 800 * attempt)); 
    }
  }

  if (!finalStreamUrl) {
    throw new Error('未能获取有效的斗鱼直播流地址。');
  }

  try {
    await invoke('set_stream_url_cmd', {
      url: finalStreamUrl,
      platform: 'douyu',
      roomId: roomId,
      quality: quality,
      line: line ?? null,
    });
    const proxyUrl = await invoke<string>('start_proxy');
    douyuProxyActive = true;
    return { streamUrl: proxyUrl, streamType };
  } catch (e: any) {
    throw new Error(`设置斗鱼代理失败: ${typeof e === 'string' ? e : (e?.message ?? String(e ?? ''))}`);
  }
}

export async function stopDouyuProxy(): Promise<void> {
  if (!douyuProxyActive) {
    return;
  }
  try {
    await invoke('stop_proxy');
    douyuProxyActive = false;
  } catch (e) {
    console.error('[DouyuPlayerHelper] Error stopping proxy server:', e);
    douyuProxyActive = false;
  }
}

function enforceHttps(url: string): string {
  if (!url) return url;
  if (url.startsWith('https://')) return url;
  if (url.startsWith('http://')) {
    return `https://${url.slice('http://'.length)}`;
  }
  return url;
}
