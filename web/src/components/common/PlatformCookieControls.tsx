"use client";

import React, { useMemo } from "react";

import { type LoginPlatform } from "@/platforms/common/platformCookie";
import { usePlatformCookie } from "@/platforms/common/usePlatformCookie";

export type PlatformCookieControlsVariant = "player" | "category";

const PLATFORM_NAME: Record<LoginPlatform, string> = {
  douyu: "斗鱼",
  huya: "虎牙"
};

type Props = {
  platform: LoginPlatform;
  variant?: PlatformCookieControlsVariant;
  onCookieChanged?: (cookie: string | null) => void;
  /** 未登录时的补充说明（例如斗鱼游客档上限）。 */
  guestHint?: string;
};

export function PlatformCookieControls(props: Props) {
  const { platform, variant = "player", onCookieChanged, guestHint } = props;
  const { hasRequired, isLoggingIn, error, login, logout } = usePlatformCookie(platform, {
    onChanged: onCookieChanged
  });

  const loginTitle = hasRequired ? "点击重新登录" : `登录${PLATFORM_NAME[platform]}`;
  const loginLabel = useMemo(() => {
    if (isLoggingIn) return "登录中...";
    if (hasRequired) return "已登录";
    return "登录";
  }, [hasRequired, isLoggingIn]);

  if (variant === "category") {
    return (
      <div style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
        <button type="button" className="category-subscribe-btn" onClick={() => void login()} disabled={isLoggingIn} title={loginTitle}>
          {loginLabel}
        </button>
        {hasRequired && !isLoggingIn ? (
          <button type="button" className="category-subscribe-btn" onClick={logout} title="退出登录">
            退出
          </button>
        ) : null}
        {error ? (
          <span className="cookie-error" title={error}>
            {error}
          </span>
        ) : null}
      </div>
    );
  }

  return (
    <span className="cookie-status">
      <button type="button" className="cookie-status-btn" onClick={() => void login()} disabled={isLoggingIn} title={loginTitle}>
        <span className={hasRequired && !isLoggingIn ? "cookie-configured" : "cookie-unset"}>{loginLabel}</span>
      </button>
      {!hasRequired && !isLoggingIn && guestHint ? (
        <span className="cookie-guest-hint">{guestHint}</span>
      ) : null}
      {hasRequired && !isLoggingIn ? (
        <button type="button" className="cookie-clear-btn" onClick={logout} title="退出登录">
          退出
        </button>
      ) : null}
      {error ? <span className="cookie-error">{error}</span> : null}
    </span>
  );
}
