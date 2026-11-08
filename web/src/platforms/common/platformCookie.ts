import { invoke } from '@tauri-apps/api/core';
import { WebviewWindow } from '@tauri-apps/api/webviewWindow';

export type LoginPlatform = 'douyu' | 'huya';

export interface PlatformCookieResult {
  platform: string;
  cookie: string | null;
  flags: Record<string, boolean>;
  hasRequired: boolean;
}

const LOGIN_WINDOW_LABEL: Record<LoginPlatform, string> = {
  douyu: 'douyu-login',
  huya: 'huya-login',
};

// 斗鱼单点登录页（已验证 200，标题「斗鱼-单点登录」）；虎牙没有独立登录页，
// 只能开首页让用户点右上角登录（扫码）。
const LOGIN_URL: Record<LoginPlatform, string> = {
  douyu: 'https://passport.douyu.com/member/login',
  huya: 'https://www.huya.com/',
};

const LOGIN_TITLE: Record<LoginPlatform, string> = {
  douyu: '斗鱼登录',
  huya: '虎牙登录',
};

export const PLATFORM_COOKIE_STORAGE_KEY: Record<LoginPlatform, string> = {
  douyu: 'douyu_cookie',
  huya: 'huya_cookie',
};

// 与 Rust 侧 platforms/common/platform_cookie.rs 的 login_names 保持一致：
// 这些 cookie 只有登录成功后才会下发，游客没有。
const LOGIN_COOKIE_NAMES: Record<LoginPlatform, string[]> = {
  douyu: ['acf_uid', 'acf_stk', 'acf_nickname'],
  huya: ['huya_nickname', 'yyb', '_mk'],
};

const normalizeResult = (platform: LoginPlatform, result: any): PlatformCookieResult => ({
  platform: String(result?.platform ?? platform),
  cookie: result?.cookie ?? null,
  flags: result?.flags ?? {},
  hasRequired: Boolean(result?.hasRequired),
});

export const getPlatformCookies = async (
  platform: LoginPlatform,
  labels?: string[]
): Promise<PlatformCookieResult> => {
  const result = await invoke<PlatformCookieResult>('get_platform_cookie', {
    platform,
    labels,
    url: LOGIN_URL[platform],
  });
  return normalizeResult(platform, result);
};

export const pushPlatformCookie = async (
  platform: LoginPlatform,
  cookie: string | null
): Promise<PlatformCookieResult> => {
  const result = await invoke<PlatformCookieResult>('set_platform_cookie', {
    platform,
    cookie: cookie && cookie.trim() ? cookie.trim() : null,
  });
  return normalizeResult(platform, result);
};

export const readStoredPlatformCookie = (platform: LoginPlatform): string => {
  try {
    return window.localStorage.getItem(PLATFORM_COOKIE_STORAGE_KEY[platform]) ?? '';
  } catch {
    return '';
  }
};

export const writeStoredPlatformCookie = (platform: LoginPlatform, cookie: string | null) => {
  try {
    const key = PLATFORM_COOKIE_STORAGE_KEY[platform];
    if (cookie && cookie.trim()) window.localStorage.setItem(key, cookie.trim());
    else window.localStorage.removeItem(key);
  } catch {
    // ignore
  }
};

export const extractLoginFlags = (platform: LoginPlatform, raw: string | null | undefined) => {
  if (!raw) return { hasRequired: false };
  const present = new Set(
    raw
      .split(';')
      .map((segment) => segment.trim())
      .filter(Boolean)
      .map((segment) => segment.split('=')[0]!.trim().toLowerCase())
  );
  const hasRequired = LOGIN_COOKIE_NAMES[platform].some((name) =>
    present.has(name.toLowerCase())
  );
  return { hasRequired };
};

export const hasRequiredCookies = (result: PlatformCookieResult | null | undefined) =>
  Boolean(result?.cookie) && Boolean(result?.hasRequired);

export const ensurePlatformLoginWindow = async (platform: LoginPlatform): Promise<WebviewWindow> => {
  const label = LOGIN_WINDOW_LABEL[platform];
  const existing = await WebviewWindow.getByLabel(label);
  if (existing) {
    try {
      await existing.show();
      await existing.setFocus();
    } catch {
      // ignore
    }
    return existing;
  }

  const loginWindow = new WebviewWindow(label, {
    url: LOGIN_URL[platform],
    title: LOGIN_TITLE[platform],
    width: 420,
    height: 640,
    resizable: true,
    focus: true,
    fullscreen: false,
    alwaysOnTop: false,
  });

  await Promise.race([
    new Promise<void>((resolve) => {
      loginWindow.once('tauri://created', () => resolve());
    }),
    new Promise<void>((_, reject) => {
      loginWindow.once('tauri://error', (event) => {
        reject(new Error(String(event.payload ?? '创建登录窗口失败')));
      });
    }),
  ]);

  try {
    await loginWindow.show();
    await loginWindow.setFocus();
  } catch {
    // ignore
  }

  return loginWindow;
};

export const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

// Rust 侧的 cookie 只在进程内存里，解析流地址（含代理续签）时从注册表读。
// 所以在模块加载时就把 localStorage 里的登录态推回去，避免冷启动第一路流
// 按游客解析（斗鱼游客只会给到蓝光4M）。
let restored = false;
export const restoreStoredPlatformCookies = () => {
  if (restored) return;
  if (typeof window === 'undefined') return;
  restored = true;
  (['douyu', 'huya'] as LoginPlatform[]).forEach((platform) => {
    const stored = readStoredPlatformCookie(platform).trim();
    if (!stored) return;
    void pushPlatformCookie(platform, stored).catch((err) => {
      console.warn(`[${platform}] 恢复登录 cookie 失败:`, err);
    });
  });
};

if (typeof window !== 'undefined') {
  restoreStoredPlatformCookies();
}
