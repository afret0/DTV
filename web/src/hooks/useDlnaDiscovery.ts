"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface DlnaDevice {
  name: string;
  location: string;
  host: string;
  control_url: string;
}

// Module-level cache shared across all component instances
let globalCache: DlnaDevice[] | null = null;
let globalDiscovering = false;
let discoverPromise: Promise<DlnaDevice[]> | null = null;
const listeners = new Set<() => void>();

function notifyListeners() {
  listeners.forEach((fn) => fn());
}

async function runDiscovery(): Promise<DlnaDevice[]> {
  if (discoverPromise) return discoverPromise;

  globalDiscovering = true;
  notifyListeners();

  discoverPromise = invoke<DlnaDevice[]>("discover_dlna_devices")
    .then((devices) => {
      globalCache = devices;
      return devices;
    })
    .catch((e) => {
      console.warn("[DLNA] Background discovery failed:", e);
      return [] as DlnaDevice[];
    })
    .finally(() => {
      globalDiscovering = false;
      discoverPromise = null;
      notifyListeners();
    });

  return discoverPromise;
}

export function useDlnaDiscovery() {
  const [, setTick] = useState(0);
  const startedRef = useRef(false);

  // Subscribe to global cache updates
  useEffect(() => {
    const listener = () => setTick((n) => n + 1);
    listeners.add(listener);
    return () => { listeners.delete(listener); };
  }, []);

  // Start discovery on first mount
  useEffect(() => {
    if (!startedRef.current) {
      startedRef.current = true;
      if (!globalCache && !globalDiscovering) {
        runDiscovery();
      }
    }
  }, []);

  const refresh = useCallback(() => {
    discoverPromise = null; // Reset to allow re-discovery
    return runDiscovery();
  }, []);

  return {
    devices: globalCache,
    discovering: globalDiscovering,
    refresh,
  };
}