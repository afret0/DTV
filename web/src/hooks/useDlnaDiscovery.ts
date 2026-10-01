"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface DlnaDevice {
  name: string;
  location: string;
  host: string;
  control_url: string;
}

// Module-level state shared by every component instance: the cast dialog must
// see what background discovery already found instead of restarting from zero.
let devicesCache: DlnaDevice[] = [];
let lastError: string | null = null;
let discovering = false;
let inFlight: Promise<DlnaDevice[]> | null = null;
const listeners = new Set<() => void>();

function notify() {
  listeners.forEach((fn) => fn());
}

function log(msg: string) {
  invoke("frontend_log", { msg }).catch(() => {});
}

function merge(found: DlnaDevice[]): DlnaDevice[] {
  const byLocation = new Map(devicesCache.map((d) => [d.location, d]));
  for (const d of found) byLocation.set(d.location, d);
  return Array.from(byLocation.values()).sort((a, b) => a.name.localeCompare(b.name));
}

async function runDiscovery(): Promise<DlnaDevice[]> {
  // Single flight: concurrent callers share one discovery instead of launching
  // overlapping network sweeps that clobber each other's state.
  if (inFlight) return inFlight;
  discovering = true;
  lastError = null;
  notify();
  log("[cast] discovery started");

  inFlight = invoke<DlnaDevice[]>("discover_dlna_devices")
    .then((found) => {
      devicesCache = merge(found ?? []);
      log(`[cast] discovery ok: ${devicesCache.length} device(s): ${devicesCache.map((d) => d.name).join(", ")}`);
      return devicesCache;
    })
    .catch((e) => {
      // Keep whatever we found before: a failed refresh must not blank the list.
      lastError = e instanceof Error ? e.message : String(e);
      log(`[cast] discovery failed: ${lastError}`);
      return devicesCache;
    })
    .finally(() => {
      discovering = false;
      inFlight = null;
      notify();
    });

  return inFlight;
}

const REFRESH_INTERVAL_MS = 15000;

export function useDlnaDiscovery() {
  const [, setTick] = useState(0);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    const listener = () => {
      if (mountedRef.current) setTick((n) => n + 1);
    };
    listeners.add(listener);
    return () => {
      mountedRef.current = false;
      listeners.delete(listener);
    };
  }, []);

  // Discover on first mount so the cast dialog opens with devices already
  // listed instead of an empty state that looks broken.
  useEffect(() => {
    void runDiscovery();
  }, []);

  const refresh = useCallback(() => runDiscovery(), []);

  return {
    devices: devicesCache,
    error: lastError,
    discovering,
    refresh,
    REFRESH_INTERVAL_MS,
  };
}

