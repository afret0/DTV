import { invoke } from "@tauri-apps/api/core";

import { applyFullLanSyncOverwrite, createLanSyncPayload, LAN_SYNC_KEYS, parseLanSyncPayload, type LanSyncPayload } from "./lanSync";

export const CLOUD_SYNC_SETTINGS_KEY = "dtv_cloud_sync_settings_v1";
export const CLOUD_SYNC_STATE_KEY = "dtv_cloud_sync_state_v1";

export interface OssSettings {
  endpoint: string;
  bucket: string;
  accessKeyId: string;
  accessKeySecret: string;
  objectKey: string;
}

export interface WebDavSettings {
  url: string;
  username: string;
  password: string;
}

export interface CloudSyncSettings {
  provider: "" | "oss" | "webdav";
  oss: OssSettings;
  webdav: WebDavSettings;
  autoSync: boolean;
  intervalHours: number;
}

export interface CloudSyncState {
  lastPushAt: number | null;
  lastPullAt: number | null;
  lastAutoSyncAt: number | null;
  localConfigAt: number | null;
  cloudConfigAt: number | null;
  lastConfigFingerprint: string | null;
  lastError: string | null;
}

export const DEFAULT_CLOUD_SYNC_SETTINGS: CloudSyncSettings = {
  provider: "",
  oss: {
    endpoint: "https://oss-cn-hangzhou.aliyuncs.com",
    bucket: "",
    accessKeyId: "",
    accessKeySecret: "",
    objectKey: "dtv/config-sync.json"
  },
  webdav: { url: "", username: "", password: "" },
  autoSync: false,
  intervalHours: 1
};

function readJson<T>(key: string): T | null {
  if (typeof window === "undefined" || !window.localStorage) return null;
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return null;
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

export function loadCloudSyncSettings(): CloudSyncSettings {
  const stored = readJson<Partial<CloudSyncSettings> & { intervalMinutes?: number }>(CLOUD_SYNC_SETTINGS_KEY);
  if (!stored) return { ...DEFAULT_CLOUD_SYNC_SETTINGS, oss: { ...DEFAULT_CLOUD_SYNC_SETTINGS.oss }, webdav: { ...DEFAULT_CLOUD_SYNC_SETTINGS.webdav } };
  const intervalHours =
    typeof stored.intervalHours === "number" && Number.isFinite(stored.intervalHours)
      ? stored.intervalHours
      : typeof stored.intervalMinutes === "number" && Number.isFinite(stored.intervalMinutes)
        ? Math.max(1, Math.round(stored.intervalMinutes / 60))
        : DEFAULT_CLOUD_SYNC_SETTINGS.intervalHours;
  return {
    ...DEFAULT_CLOUD_SYNC_SETTINGS,
    ...stored,
    intervalHours,
    oss: { ...DEFAULT_CLOUD_SYNC_SETTINGS.oss, ...(stored.oss ?? {}) },
    webdav: { ...DEFAULT_CLOUD_SYNC_SETTINGS.webdav, ...(stored.webdav ?? {}) }
  };
}

export function saveCloudSyncSettings(settings: CloudSyncSettings) {
  if (typeof window === "undefined" || !window.localStorage) return;
  window.localStorage.setItem(CLOUD_SYNC_SETTINGS_KEY, JSON.stringify(settings));
}

export function loadCloudSyncState(): CloudSyncState {
  const stored = readJson<Partial<CloudSyncState> & { lastAutoPushAt?: number | null }>(CLOUD_SYNC_STATE_KEY);
  return {
    lastPushAt: stored?.lastPushAt ?? null,
    lastPullAt: stored?.lastPullAt ?? null,
    lastAutoSyncAt: stored?.lastAutoSyncAt ?? stored?.lastAutoPushAt ?? null,
    localConfigAt: stored?.localConfigAt ?? null,
    cloudConfigAt: stored?.cloudConfigAt ?? null,
    lastConfigFingerprint: stored?.lastConfigFingerprint ?? null,
    lastError: stored?.lastError ?? null
  };
}

export function saveCloudSyncState(state: CloudSyncState) {
  if (typeof window === "undefined" || !window.localStorage) return;
  window.localStorage.setItem(CLOUD_SYNC_STATE_KEY, JSON.stringify(state));
}

function patchState(patch: Partial<CloudSyncState>): CloudSyncState {
  const next = { ...loadCloudSyncState(), ...patch };
  saveCloudSyncState(next);
  return next;
}

export function buildCloudPayload(appVersion?: string | null): LanSyncPayload {
  return createLanSyncPayload(window.localStorage, { client: "desktop", appVersion: appVersion ?? null });
}

export async function cloudSyncPush(appVersion?: string | null): Promise<string> {
  const settings = loadCloudSyncSettings();
  const payload = buildCloudPayload(appVersion);
  const message = (await invoke<string>("cloud_sync_push", {
    settings,
    payloadJson: JSON.stringify(payload)
  })) as string;
  const now = Date.now();
  patchState({ lastPushAt: now, localConfigAt: now, lastConfigFingerprint: configFingerprint(), lastError: null });
  return message;
}

export async function cloudSyncPull(appVersion?: string | null): Promise<string> {
  const settings = loadCloudSyncSettings();
  const remote = (await invoke<string | null>("cloud_sync_pull", { settings })) as string | null;
  if (!remote) {
    return "远端还没有配置文件，先点「上传到云端」";
  }
  const payload = parseLanSyncPayload(JSON.parse(remote));
  const result = applyFullLanSyncOverwrite(window.localStorage, payload.entries);
  const now = Date.now();
  patchState({
    lastPullAt: now,
    localConfigAt: now,
    cloudConfigAt: Date.parse(payload.exportedAt),
    lastConfigFingerprint: configFingerprint(),
    lastError: null
  });
  void appVersion;
  return `已从云端完整覆盖：写入 ${result.appliedKeys} 项、删除 ${result.removedKeys} 项`;
}

export async function cloudSyncTest(): Promise<string> {
  const settings = loadCloudSyncSettings();
  return (await invoke<string>("cloud_sync_test", { settings })) as string;
}

function configFingerprint(): string {
  const parts = LAN_SYNC_KEYS.map((key) => `${key}=${window.localStorage.getItem(key) ?? ""}`);
  let hash = 5381;
  const text = parts.join("\u0001");
  for (let i = 0; i < text.length; i += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(i)) | 0;
  }
  return `${text.length}:${hash}`;
}

/** 本地配置内容被改过（指纹变化）时把本地时间戳推到 now，让本机的修改能参与「谁新覆盖谁」。 */
export function refreshLocalConfigTimestamp(): boolean {
  const fingerprint = configFingerprint();
  const state = loadCloudSyncState();
  if (state.lastConfigFingerprint === fingerprint) return false;
  if (state.localConfigAt == null) {
    patchState({ lastConfigFingerprint: fingerprint });
    return false;
  }
  patchState({ localConfigAt: Date.now(), lastConfigFingerprint: fingerprint });
  return true;
}

/**
 * 自动同步心跳：定时读云端配置，比较云端 exportedAt 与本地时间戳，新的覆盖旧的。
 * 云端新 → 完整覆盖本地；本地新 → 整份推上云；相等或读不到时间戳 → 不动。
 */
export async function autoCloudSyncTick(appVersion?: string | null): Promise<string | null> {
  const settings = loadCloudSyncSettings();
  if (!settings.provider || !settings.autoSync) return null;
  refreshLocalConfigTimestamp();
  const state = loadCloudSyncState();
  const intervalMs = Math.max(1, Math.round(settings.intervalHours)) * 3_600_000;
  if (Date.now() - (state.lastAutoSyncAt ?? 0) < intervalMs) return null;
  try {
    const remote = (await invoke<string | null>("cloud_sync_pull", { settings })) as string | null;
    const now = Date.now();
    if (!remote) {
      await cloudSyncPush(appVersion);
      patchState({ lastAutoSyncAt: now, cloudConfigAt: now });
      return "云端还没有配置文件，已把本机配置上传";
    }
    const payload = parseLanSyncPayload(JSON.parse(remote));
    const cloudAt = Date.parse(payload.exportedAt);
    const localAt = loadCloudSyncState().localConfigAt;
    patchState({ cloudConfigAt: cloudAt });
    if (localAt == null) {
      patchState({ lastAutoSyncAt: now, localConfigAt: cloudAt, lastError: null });
      return "本地还没有时间戳，以云端配置时间初始化基线，本轮不覆盖";
    }
    if (cloudAt > localAt) {
      const result = applyFullLanSyncOverwrite(window.localStorage, payload.entries);
      patchState({
        lastPullAt: now,
        lastAutoSyncAt: now,
        localConfigAt: now,
        lastConfigFingerprint: configFingerprint(),
        lastError: null
      });
      return `云端配置更新，已覆盖本机：写入 ${result.appliedKeys} 项、删除 ${result.removedKeys} 项`;
    }
    if (localAt > cloudAt) {
      await cloudSyncPush(appVersion);
      patchState({ lastAutoSyncAt: now, cloudConfigAt: now, lastError: null });
      return "本机配置更新，已上传覆盖云端";
    }
    patchState({ lastAutoSyncAt: now, lastError: null });
    return "两端时间戳一致，无需同步";
  } catch (error) {
    patchState({ lastError: String((error as any)?.message ?? error) });
    return null;
  }
}
