import { invoke } from '@tauri-apps/api/core';
import { listen, type Event as TauriEvent } from '@tauri-apps/api/event';
import type { LiveStreamInfo, StreamVariant } from '../common/types';
import type { Ref } from '../common/ref';
import type { DanmakuMessage, DanmuOverlayInstance, DanmuRenderOptions } from '../../components/player/types';
import { v4 as uuidv4 } from 'uuid';
import { logger } from '@/utils/logger';

export async function getBilibiliStreamConfig(
  roomId: string,
  quality: string = '原画',
  cookie?: string,
): Promise<{ streamUrl: string, streamType: string | undefined }> {
  if (!roomId) {
    throw new Error('房间ID未提供');
  }
  const payloadData = { args: { room_id_str: roomId } };
  // 若未显式传入 cookie，则尝试从 localStorage 读取，以确保最高画质可用
  const effectiveCookie = cookie ?? (typeof localStorage !== 'undefined' ? (localStorage.getItem('bilibili_cookie') || undefined) : undefined);

  const MAX_ATTEMPTS = 2; // 最多重试一次
  let result: LiveStreamInfo | null = null;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    try {
      const fetched = await invoke<LiveStreamInfo>('get_bilibili_live_stream_url_with_quality', {
        payload: payloadData,
        quality,
        cookie: effectiveCookie || null,
      });
      result = fetched;

      // 后端返回错误：只有明确写了"未开播"才算离线；其余（风控 -352、
      // cookie 失效、网络抖动等）是可重试的瞬时失败，文案不能带"未开播"，
      // 否则前端会停止自动重试并把在线房间展示成离线。
      if (result.error_message) {
        const msg = result.error_message.trim();
        if (msg.includes('未开播')) {
          throw new Error(msg);
        }
        throw new Error(`BILIBILI_TRANSIENT:${msg.slice(0, 160)}`);
      }

      // 根据返回的状态判断是否在线（B 站约定 status === 1 为在线）
      if (typeof result.status !== 'undefined' && result.status !== 1) {
        throw new Error('主播未开播');
      }

      // status 显示在线却没有播放地址：视为瞬时失败重试
      if (!result.stream_url) {
        throw new Error('BILIBILI_TRANSIENT:直播流地址为空');
      }

      break;
    } catch (e: any) {
      // Tauri invoke 失败时 reject 的是字符串本身，e.message 为 undefined
      const msg = (typeof e === 'string' ? e : (e?.message ? String(e.message) : String(e ?? ''))).trim();
      const looksOffline = msg.includes('未开播') || msg.includes('房间不存在');
      if (looksOffline) {
        throw new Error(msg || '主播未开播');
      }
      if (attempt >= MAX_ATTEMPTS) {
        const detail = msg.startsWith('BILIBILI_TRANSIENT:') ? msg.slice('BILIBILI_TRANSIENT:'.length) : msg;
        throw new Error(`获取B站直播流失败（网络或服务端繁忙）${detail ? ': ' + detail.slice(0, 120) : ''}`);
      }
      await new Promise((resolve) => setTimeout(resolve, 600 * attempt));
    }
  }

  if (!result) {
    throw new Error('获取B站直播流失败（网络或服务端繁忙）');
  }

  // 调试输出：真实上游地址与所有可用地址
  if (result.upstream_url) {
    logger.debug('[Bilibili] 上游真实地址（可用于 VLC 测试）:', result.upstream_url);
  }
  if (result.available_streams && Array.isArray(result.available_streams)) {
    logger.debug(`[Bilibili] 可用播放地址（共 ${result.available_streams.length} 条）:`);
    (result.available_streams as StreamVariant[]).forEach((v, idx) => {
      const meta = [v.format, v.desc, v.qn?.toString(), v.protocol].filter(Boolean).join(' | ');
      logger.debug(`  [${idx + 1}] ${v.url}${meta ? `  <<< ${meta}` : ''}`);
    });
  }

  const streamUrl = result.stream_url;
  if (!streamUrl) {
    throw new Error('获取B站直播流失败（网络或服务端繁忙）');
  }

  let streamType: string | undefined;
  const streamUrlLower = streamUrl.toLowerCase();

  if (
    streamUrlLower.startsWith('http://127.0.0.1') ||
    streamUrlLower.includes('/live.flv') ||
    streamUrlLower.includes('.flv')
  ) {
    streamType = 'flv';
  } else if (streamUrlLower.includes('.m3u8')) {
    streamType = 'hls';
  }

  if (!streamType && result.available_streams && Array.isArray(result.available_streams)) {
    const matchedVariant = (result.available_streams as StreamVariant[]).find((variant) => {
      if (!variant?.url) {
        return false;
      }
      const formatLower = variant.format?.toLowerCase() ?? '';
      const protocolLower = variant.protocol?.toLowerCase() ?? '';
      const isSameAsPrimary =
        variant.url === streamUrl || variant.url === result.upstream_url;
      const isHlsCandidate =
        formatLower === 'ts' ||
        formatLower === 'fmp4' ||
        formatLower === 'mp4' ||
        formatLower === 'm4s' ||
        protocolLower.includes('hls');
      return isSameAsPrimary && isHlsCandidate;
    });
    if (matchedVariant) {
      streamType = 'hls';
    }
  }

  if (!streamType && result.upstream_url) {
    const upstreamLower = result.upstream_url.toLowerCase();
    if (upstreamLower.includes('.m3u8')) {
      streamType = 'hls';
    } else if (
      upstreamLower.startsWith('http://127.0.0.1') ||
      upstreamLower.includes('/live.flv') ||
      upstreamLower.includes('.flv')
    ) {
      streamType = 'flv';
    }
  }

  if (!streamType) {
    streamType = 'flv';
  }

  // The Rust command already stores the real upstream URL, starts the local
  // proxy and returns the proxy URL for FLV (Referer is injected server-side).
  // Writing the returned proxy URL back into the store would make the proxy
  // pump connect to itself, so we must not touch the store here.
  if (streamUrl.startsWith('http://127.0.0.1') || streamUrl.includes('/live.flv')) {
    biliProxyActive = true;
  }

  return { streamUrl, streamType };
}

// 统一的 Rust 弹幕事件负载（与 Douyin/Douyu/Huya 保持一致）
interface UnifiedRustDanmakuPayload {
  room_id: string;
  user: string;
  content: string;
  user_level: number;
  fans_club_level: number;
}

export async function startBilibiliDanmakuListener(
  roomId: string,
  danmuOverlay: DanmuOverlayInstance | null,
  danmakuMessagesRef: Ref<DanmakuMessage[]>,
  cookie?: string,
  renderOptions?: DanmuRenderOptions,
): Promise<() => void> {
  // 启动后端 B 站弹幕监听（cookie 可选）；若未传，则从 localStorage 兜底读取
  const effectiveCookie = cookie ?? (typeof localStorage !== 'undefined' ? (localStorage.getItem('bilibili_cookie') || undefined) : undefined);
  await invoke('start_bilibili_danmaku_listener', {
    payload: { args: { room_id_str: roomId } },
    cookie: effectiveCookie || null,
  });

  const eventName = 'danmaku-message';
  const unlisten = await listen<UnifiedRustDanmakuPayload>(eventName, (event: TauriEvent<UnifiedRustDanmakuPayload>) => {
    if (!event.payload || event.payload.room_id !== roomId) return;

    const frontendDanmaku: DanmakuMessage = {
      id: uuidv4(),
      nickname: event.payload.user || '未知用户',
      content: event.payload.content,
      level: String(event.payload.user_level ?? 0),
      badgeLevel: event.payload.fans_club_level != null ? String(event.payload.fans_club_level) : undefined,
      room_id: roomId,
    };

    const shouldDisplay = renderOptions?.shouldDisplay ? renderOptions.shouldDisplay(frontendDanmaku) : true;

    if (shouldDisplay && danmuOverlay?.sendComment) {
      try {
        const commentOptions = renderOptions?.buildCommentOptions?.(frontendDanmaku) ?? {};
        const styleFromOptions = commentOptions.style ?? {};
        const preferredColor = styleFromOptions.color || (frontendDanmaku as any).color || '#FFFFFF';
        danmuOverlay.sendComment({
          id: frontendDanmaku.id,
          txt: frontendDanmaku.content,
          duration: commentOptions.duration ?? 12000,
          mode: commentOptions.mode ?? 'scroll',
          style: {
            ...styleFromOptions,
            color: preferredColor,
          },
        });
      } catch (emitError) {
        console.warn('[BilibiliPlayerHelper] Failed emitting danmu.js comment:', emitError);
      }
    }

    const shouldAppend = renderOptions?.shouldAppendToList ? renderOptions.shouldAppendToList(frontendDanmaku) : true;
    if (shouldAppend) {
      danmakuMessagesRef.value.push(frontendDanmaku);
      if (danmakuMessagesRef.value.length > 200) {
        danmakuMessagesRef.value.splice(0, danmakuMessagesRef.value.length - 200);
      }
    }
  });
  return unlisten;
}

export async function stopBilibiliDanmaku(currentUnlistenFn: (() => void) | null): Promise<void> {
  if (currentUnlistenFn) {
    try { currentUnlistenFn(); } catch {}
  }
  try {
    await invoke('stop_bilibili_danmaku_listener');
  } catch {}
}

let biliProxyActive = false;

export async function stopBilibiliProxy(): Promise<void> {
  if (!biliProxyActive) return;
  try { await invoke('stop_proxy'); biliProxyActive = false; } catch {}
}
