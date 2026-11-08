"use client";

import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { Platform } from "@/platforms/common/types";

export const ROOM_SWITCH_TIMEOUT_MS = 8000;

export type RoomSwitchState = {
  key: string;
  startedAt: number;
};

function logRoomSwitch(msg: string) {
  try {
    void invoke("frontend_log", { msg }).catch(() => {});
  } catch {}
}

export function roomSwitchKey(platform: string, roomId: string) {
  return `${(platform || "").toLowerCase()}:${roomId || ""}`;
}

export type PlayerIslandState = {
  visible: boolean;
  platform: Platform | null;
  roomId: string | null;
  anchorName: string | null;
  title: string | null;
  avatarUrl: string | null;
};

export type DanmuPanelState = {
  available: boolean;
  collapsed: boolean;
};

type PlayerUiContextValue = {
  island: PlayerIslandState;
  setIsland: (next: Partial<PlayerIslandState>) => void;
  clearIsland: () => void;
  isFullscreen: boolean;
  setFullscreen: (next: boolean) => void;
  danmuPanel: DanmuPanelState;
  setDanmuPanel: (next: DanmuPanelState) => void;
  registerDanmuPanelSetter: (setter: ((visible: boolean) => void) | null) => void;
  requestShowDanmuPanel: () => void;
  roomSwitch: RoomSwitchState | null;
  beginRoomSwitch: (key: string) => boolean;
  endRoomSwitch: (key: string, reason: string) => void;
  cancelRoomSwitch: (reason: string) => void;
};

const PlayerUiContext = createContext<PlayerUiContextValue | null>(null);

const emptyIsland: PlayerIslandState = {
  visible: false,
  platform: null,
  roomId: null,
  anchorName: null,
  title: null,
  avatarUrl: null
};

const emptyDanmuPanel: DanmuPanelState = {
  available: false,
  collapsed: true
};

export function PlayerUiProvider({ children }: { children: React.ReactNode }) {
  const [island, setIslandState] = useState<PlayerIslandState>(emptyIsland);
  const [isFullscreen, setIsFullscreen] = useState(false);
  const [danmuPanel, setDanmuPanelState] = useState<DanmuPanelState>(emptyDanmuPanel);
  const danmuSetterRef = useRef<((visible: boolean) => void) | null>(null);
  const [roomSwitch, setRoomSwitch] = useState<RoomSwitchState | null>(null);
  const roomSwitchRef = useRef<RoomSwitchState | null>(null);

  const beginRoomSwitch = useCallback((key: string) => {
    const prev = roomSwitchRef.current;
    const now = Date.now();
    if (prev && now - prev.startedAt < ROOM_SWITCH_TIMEOUT_MS) {
      if (prev.key === key) {
        logRoomSwitch(`[ui] room switch ignored, 同一房间正在切换中, key: ${key}, elapsed: ${now - prev.startedAt}ms`);
        return false;
      }
      logRoomSwitch(`[ui] room switch retarget, ${prev.key} -> ${key}, elapsed: ${now - prev.startedAt}ms`);
    }
    const next: RoomSwitchState = { key, startedAt: now };
    roomSwitchRef.current = next;
    setRoomSwitch(next);
    logRoomSwitch(`[ui] room switch begin, key: ${key}`);
    return true;
  }, []);

  const endRoomSwitch = useCallback((key: string, reason: string) => {
    const prev = roomSwitchRef.current;
    if (!prev || prev.key !== key) return;
    roomSwitchRef.current = null;
    setRoomSwitch(null);
    logRoomSwitch(`[ui] room switch end, key: ${key}, reason: ${reason}, cost: ${Date.now() - prev.startedAt}ms`);
  }, []);

  const cancelRoomSwitch = useCallback((reason: string) => {
    const prev = roomSwitchRef.current;
    roomSwitchRef.current = null;
    setRoomSwitch(null);
    if (prev) logRoomSwitch(`[ui] room switch cancel, key: ${prev.key}, reason: ${reason}`);
  }, []);

  useEffect(() => {
    const pending = roomSwitchRef.current;
    if (!roomSwitch || !pending || pending.key !== roomSwitch.key) return;
    const remain = ROOM_SWITCH_TIMEOUT_MS - (Date.now() - pending.startedAt);
    const timer = window.setTimeout(() => {
      endRoomSwitch(pending.key, "timeout");
    }, Math.max(0, remain));
    return () => window.clearTimeout(timer);
  }, [endRoomSwitch, roomSwitch]);

  const setIsland = useCallback((next: Partial<PlayerIslandState>) => {
    setIslandState((prev) => ({ ...prev, ...next, visible: true }));
  }, []);

  const clearIsland = useCallback(() => {
    setIslandState(emptyIsland);
  }, []);

  const registerDanmuPanelSetter = useCallback((setter: ((visible: boolean) => void) | null) => {
    danmuSetterRef.current = setter;
  }, []);

  const requestShowDanmuPanel = useCallback(() => {
    danmuSetterRef.current?.(true);
  }, []);

  const setDanmuPanel = useCallback((next: DanmuPanelState) => {
    setDanmuPanelState(next);
  }, []);

  const value = useMemo<PlayerUiContextValue>(() => {
    return {
      island,
      setIsland,
      clearIsland,
      isFullscreen,
      setFullscreen: setIsFullscreen,
      danmuPanel,
      setDanmuPanel,
      registerDanmuPanelSetter,
      requestShowDanmuPanel,
      roomSwitch,
      beginRoomSwitch,
      endRoomSwitch,
      cancelRoomSwitch
    };
  }, [
    beginRoomSwitch,
    cancelRoomSwitch,
    clearIsland,
    danmuPanel,
    endRoomSwitch,
    island,
    isFullscreen,
    registerDanmuPanelSetter,
    requestShowDanmuPanel,
    roomSwitch,
    setDanmuPanel,
    setIsland
  ]);

  return <PlayerUiContext.Provider value={value}>{children}</PlayerUiContext.Provider>;
}

export function usePlayerUi() {
  const ctx = useContext(PlayerUiContext);
  if (!ctx) throw new Error("usePlayerUi must be used within PlayerUiProvider");
  return ctx;
}
