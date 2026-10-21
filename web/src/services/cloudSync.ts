import { invoke } from "@tauri-apps/api/core";

import { applyIncrementalLanSyncImport, createLanSyncPayload, type LanSyncPayload } from "./lanSync";

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
  intervalMinutes: number;
}

export interface CloudSyncState {
  lastPushAt: number | null;
  lastPullAt: number | null;
  lastAutoPushAt: number | null;
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
  intervalMinutes: 60
};

export const SYNC_INTERVAL_OPTIONS = [
  { value: 15, label: "每 15 分钟" },
  { value: 30, label: "每 30 分钟" },
  { value: 60, label: "每 1 小时" },
  { value: 180, label: "每 3 小时" },
  { value: 360, label: "每 6 小时" },
  { value: 720, label: "每 12 小时" },
  { value: 1440, label: "每 24 小时" }
] as const;

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
  const stored = readJson<Partial<CloudSyncSettings>>(CLOUD_SYNC_SETTINGS_KEY);
  if (!stored) return { ...DEFAULT_CLOUD_SYNC_SETTINGS, oss: { ...DEFAULT_CLOUD_SYNC_SETTINGS.oss }, webdav: { ...DEFAULT_CLOUD_SYNC_SETTINGS.webdav } };
  return {
    ...DEFAULT_CLOUD_SYNC_SETTINGS,
    ...stored,
    oss: { ...DEFAULT_CLOUD_SYNC_SETTINGS.oss, ...(stored.oss ?? {}) },
    webdav: { ...DEFAULT_CLOUD_SYNC_SETTINGS.webdav, ...(stored.webdav ?? {}) }
  };
}

export function saveCloudSyncSettings(settings: CloudSyncSettings) {
  if (typeof window === "undefined" || !window.localStorage) return;
  window.localStorage.setItem(CLOUD_SYNC_SETTINGS_KEY, JSON.stringify(settings));
}

export function loadCloudSyncState(): CloudSyncState {
  return (
    readJson<CloudSyncState>(CLOUD_SYNC_STATE_KEY) ?? {
      lastPushAt: null,
      lastPullAt: null,
      lastAutoPushAt: null,
      lastError: null
    }
  );
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
  patchState({ lastPushAt: Date.now(), lastError: null });
  return message;
}

export async function cloudSyncPull(appVersion?: string | null): Promise<string> {
  const settings = loadCloudSyncSettings();
  const remote = (await invoke<string | null>("cloud_sync_pull", { settings })) as string | null;
  if (!remote) {
    return "远端还没有配置文件，先点「上传到云端」";
  }
  const payload = JSON.parse(remote) as LanSyncPayload;
  const result = applyIncrementalLanSyncImport(window.localStorage, payload.entries);
  patchState({ lastPullAt: Date.now(), lastError: null });
  const parts: string[] = [];
  if (result.addedStreamers) parts.push(`关注 +${result.addedStreamers}`);
  if (result.addedFolderCount) parts.push(`分组 +${result.addedFolderCount}`);
  if (result.addedCustomCategories) parts.push(`自定义分类 +${result.addedCustomCategories}`);
  if (result.addedDanmuBlockKeywords) parts.push(`屏蔽词 +${result.addedDanmuBlockKeywords}`);
  if (result.appliedDanmuPreferences) parts.push("弹幕偏好");
  void appVersion;
  return `已从云端应用：${parts.length ? parts.join("、") : "无变化"}`;
}

export async function cloudSyncTest(): Promise<string> {
  const settings = loadCloudSyncSettings();
  return (await invoke<string>("cloud_sync_test", { settings })) as string;
}

/** 自动同步心跳：到点就把本地配置推上云。返回是否真的推了。 */
export async function maybeAutoCloudSync(appVersion?: string | null): Promise<boolean> {
  const settings = loadCloudSyncSettings();
  if (!settings.provider || !settings.autoSync) return false;
  const state = loadCloudSyncState();
  const intervalMs = Math.max(5, settings.intervalMinutes) * 60_000;
  const last = state.lastAutoPushAt ?? 0;
  if (Date.now() - last < intervalMs) return false;
  try {
    await cloudSyncPush(appVersion);
    patchState({ lastAutoPushAt: Date.now() });
    return true;
  } catch (error) {
    patchState({ lastError: String((error as any)?.message ?? error) });
    return false;
  }
}
