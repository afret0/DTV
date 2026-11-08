"use client";

import { type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  closeWindowByLabel,
  ensurePlatformLoginWindow,
  extractLoginFlags,
  getPlatformCookies,
  hasRequiredCookies,
  pushPlatformCookie,
  readStoredPlatformCookie,
  sleep,
  writeStoredPlatformCookie,
  type LoginPlatform
} from "@/platforms/common/platformCookie";

type Options = {
  onChanged?: (cookie: string | null) => void;
};

export type PlatformCookieState = {
  cookie: string;
  hasRequired: boolean;
  isLoggingIn: boolean;
  error: string | null;
};

export function usePlatformCookie(platform: LoginPlatform, options: Options = {}) {
  const { onChanged } = options;

  const [cookie, setCookie] = useState("");
  const [hasRequired, setHasRequired] = useState(false);
  const [isLoggingIn, setIsLoggingIn] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const mountedRef = useRef(true);
  const onChangedRef = useRef<Options["onChanged"]>(onChanged);
  useEffect(() => {
    onChangedRef.current = onChanged;
  }, [onChanged]);

  const updateFromRaw = useCallback((raw: string | null | undefined) => {
    if (!mountedRef.current) return;
    const value = String(raw ?? "").trim();
    setCookie(value);
    setHasRequired(extractLoginFlags(platform, value).hasRequired);
  }, [platform]);

  const persist = useCallback(
    (raw: string | null | undefined) => {
      const value = String(raw ?? "").trim();
      writeStoredPlatformCookie(platform, value || null);
      updateFromRaw(value);
      // Rust 侧解析流地址按进程内注册表读 cookie，登录态一变就同步过去，
      // 代理续签才不会再退回游客档位。
      void pushPlatformCookie(platform, value || null).catch((err) => {
        console.warn(`[${platform}] 同步登录 cookie 到 Rust 失败:`, err);
      });
      try {
        onChangedRef.current?.(value ? value : null);
      } catch {
        // ignore
      }
    },
    [platform, updateFromRaw]
  );

  const loadFromStorage = useCallback(() => {
    updateFromRaw(readStoredPlatformCookie(platform));
  }, [platform, updateFromRaw]);

  useEffect(() => {
    mountedRef.current = true;
    loadFromStorage();
    const stored = readStoredPlatformCookie(platform).trim();
    if (stored) {
      void pushPlatformCookie(platform, stored).catch(() => {
        // ignore
      });
    }
    return () => {
      mountedRef.current = false;
    };
  }, [loadFromStorage, platform]);

  const logout = useCallback(() => {
    setError(null);
    persist(null);
  }, [persist]);

  const login = useCallback(async () => {
    if (isLoggingIn) return;
    setError(null);
    setIsLoggingIn(true);

    let unlisten: UnlistenFn | null = null;
    try {
      const loginWindow = await ensurePlatformLoginWindow(platform);
      let windowClosed = false;

      unlisten = await loginWindow.listen("tauri://close-requested", () => {
        windowClosed = true;
      });

      const timeoutMs = 180_000;
      const intervalMs = 1_500;
      const deadline = Date.now() + timeoutMs;

      while (!windowClosed && Date.now() < deadline) {
        // 不限定窗口：虎牙/斗鱼的扫码页可能在新 webview 里打开，
        // 全窗口扫一遍才拿得到登录 cookie。
        const result = await getPlatformCookies(platform);
        if (hasRequiredCookies(result)) {
          persist(result.cookie);
          // 登录完成就关窗：前端对非 main 窗口没有 close 权限，走 Rust 兜底。
          const closedByRust = await closeWindowByLabel(loginWindow.label);
          if (!closedByRust) {
            try {
              await loginWindow.close();
            } catch (closeErr) {
              console.warn(`[${platform}] 关闭登录窗口失败:`, closeErr);
            }
          }
          return;
        }
        await sleep(intervalMs);
      }

      if (windowClosed) {
        throw new Error("登录窗口已关闭，未完成登录");
      }
      throw new Error("登录超时，请重试");
    } catch (e: any) {
      const msg = e?.message || "登录失败，请重试";
      if (mountedRef.current) setError(msg);
      console.error(`[${platform}] 登录失败:`, e);
    } finally {
      if (unlisten) {
        try {
          unlisten();
        } catch {
          // ignore
        }
      }
      if (mountedRef.current) setIsLoggingIn(false);
    }
  }, [isLoggingIn, persist, platform]);

  const state = useMemo<PlatformCookieState>(
    () => ({ cookie, hasRequired, isLoggingIn, error }),
    [cookie, error, hasRequired, isLoggingIn]
  );

  return { ...state, loadFromStorage, persist, login, logout };
}
