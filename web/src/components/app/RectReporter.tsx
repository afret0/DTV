"use client";
import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";

function report() {
  try {
    const out: Record<string, unknown> = {};
    const tabs = Array.from(document.querySelectorAll("button")).filter((b) => {
      const t = (b.textContent || "").trim();
      return ["斗鱼", "虎牙", "抖音", "B站"].includes(t);
    });
    out.tabs = tabs.map((b) => {
      const r = b.getBoundingClientRect();
      return { name: (b.textContent || "").trim(), x: r.x + r.width / 2, y: r.y + r.height / 2 };
    });
    // RectReporter's job is to hand a synthetic-click harness real coordinates
    // for the MAIN room grid, not for the sidebar's small list items (which
    // also match a generic `[role="button"]` query). Match live-room cards by
    // the same viewer-count text the desktop AX path uses, and require card-like
    // dimensions.
    const cards = Array.from(document.querySelectorAll('[role="button"], button, div'))
      .filter((el) => {
        const r = (el as HTMLElement).getBoundingClientRect();
        if (r.width < 60 || r.height < 60) return false;
        const t = (el.textContent || "");
        return /观看人数|人气|在线人数|人正在看/.test(t);
      })
      .slice(0, 4);
    out.cards = cards.map((c, i) => {
      const r = (c as HTMLElement).getBoundingClientRect();
      return { i, x: r.x + r.width / 2, y: r.y + r.height / 2, w: r.width, h: r.height };
    });
    out.viewport = { w: window.innerWidth, h: window.innerHeight };
    out.playerBar = !!document.querySelector(".xgplayer");
    const close = document.querySelector('[aria-label="关闭播放器"]') as HTMLElement | null;
    if (close) {
      const r = close.getBoundingClientRect();
      out.close = { x: r.x + r.width / 2, y: r.y + r.height / 2 };
    }
    const err = document.querySelector(".retry-btn") as HTMLElement | null;
    if (err) {
      const r = err.getBoundingClientRect();
      out.retry = { x: r.x + r.width / 2, y: r.y + r.height / 2 };
    }
    // Cast dialog automation hooks (temporary diagnostics).
    const cast = document.querySelector(".xgplayer-cast-control") as HTMLElement | null;
    if (cast) {
      const r = cast.getBoundingClientRect();
      out.castBtn = { x: r.x + r.width / 2, y: r.y + r.height / 2, w: r.width, h: r.height };
    }
    const inputs = Array.from(document.querySelectorAll<HTMLInputElement>('input[placeholder*="电视IP"]'));
    if (inputs[0]) {
      const r = inputs[0].getBoundingClientRect();
      out.castIpInput = { x: r.x + r.width / 2, y: r.y + r.height / 2 };
    }
    // Locate the cast dialog by its title text, then report its buttons so a
    // synthetic click can drive the real cast flow.
    // The dialog backdrop is the fixed overlay React renders with zIndex 300;
    // scoping to it keeps page buttons out of the reported list.
    const dl = Array.from(document.querySelectorAll("div"))
      .filter((d) => (d as HTMLElement).style?.zIndex === "300")
      .filter((d) => (d as HTMLElement).textContent?.includes("投屏到电视"))
      .sort((a, b) => b.querySelectorAll("*").length - a.querySelectorAll("*").length)[0] as
      | HTMLElement
      | undefined;
    if (dl) {
      const btns = Array.from(dl.querySelectorAll("button")).slice(0, 12).map((b, i) => {
        const r = b.getBoundingClientRect();
        return {
          i,
          x: r.x + r.width / 2,
          y: r.y + r.height / 2,
          w: Math.round(r.width),
          h: Math.round(r.height),
          text: (b.textContent || "").trim().slice(0, 24),
        };
      });
      out.castDialog = { btns, text: (dl.textContent || "").replace(/\s+/g, " ").slice(0, 160) };
    }
    invoke("frontend_log", { msg: "RECTS " + JSON.stringify(out) }).catch(() => {});
  } catch {}
}

export function RectReporter() {
  useEffect(() => {
    let id: number | undefined;
    invoke<boolean>("rect_debug_enabled")
      .then((on) => {
        if (on) {
          report();
          id = window.setInterval(report, 2500);
        }
      })
      .catch(() => {});
    return () => {
      if (id) window.clearInterval(id);
    };
  }, []);
  return null;
}
