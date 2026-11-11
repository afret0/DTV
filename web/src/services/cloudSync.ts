import { invoke } from "@tauri-apps/api/core";

import {
  applyFullLanSyncOverwrite,
  buildCloudLedgerSections,
  collectSyncItemValues,
  createLanSyncPayload,
  CLOUD_LOCAL_ONLY_KEYS,
  CLOUD_REV_INCREMENTAL,
  CLOUD_SYNC_KEYS,
  parseCloudLedger,
  notifyConfigMerged,
  parseLanSyncPayload,
  reconcileCloudSync,
  stableConfigFingerprint,
  type CloudLedger,
  type LanSyncPayload
} from "./lanSync";

export const CLOUD_SYNC_SETTINGS_KEY = "dtv_cloud_sync_settings_v1";
export const CLOUD_SYNC_STATE_KEY = "dtv_cloud_sync_state_v1";
export const CLOUD_SYNC_LEDGER_KEY = "dtv_cloud_sync_ledger_v1";

export const CHANGE_TRIGGER_KEYS: readonly string[] = CLOUD_SYNC_KEYS;

export const CHANGE_SYNC_DEBOUNCE_MS = 5_000;

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
  lastChangeSyncAt: number | null;
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
    lastChangeSyncAt: stored?.lastChangeSyncAt ?? null,
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

function loadCloudLedger(): CloudLedger {
  return parseCloudLedger(readJson<unknown>(CLOUD_SYNC_LEDGER_KEY));
}

export function saveCloudLedger(ledger: CloudLedger) {
  if (typeof window === "undefined" || !window.localStorage) return;
  try {
    window.localStorage.setItem(CLOUD_SYNC_LEDGER_KEY, JSON.stringify(ledger));
  } catch {
  }
}

function ledgerSeedAt(): number | null {
  const ledger = loadCloudLedger();
  const hasItems = Object.values(ledger.items).some((bucket) => Object.keys(bucket).length > 0);
  if (hasItems) return null;
  return loadCloudSyncState().localConfigAt;
}

export function buildCloudPayload(appVersion?: string | null): LanSyncPayload {
  const base = createLanSyncPayload(window.localStorage, { client: "desktop", appVersion: appVersion ?? null });
  for (const localOnlyKey of CLOUD_LOCAL_ONLY_KEYS) delete base.entries[localOnlyKey];
  const now = Date.now();
  const { ledger, itemAt, deletions } = buildCloudLedgerSections(loadCloudLedger(), collectSyncItemValues(window.localStorage), now, ledgerSeedAt());
  saveCloudLedger(ledger);
  return { ...base, rev: CLOUD_REV_INCREMENTAL, itemAt, deletions };
}

function applyCloudPayload(payload: LanSyncPayload) {
  const now = Date.now();
  const result = reconcileCloudSync(window.localStorage, payload, loadCloudLedger(), now, ledgerSeedAt());
  saveCloudLedger(result.ledger);
  return { changed: result.changed, ...result.stats, now };
}

function reStampLedger(now: number) {
  const { ledger } = buildCloudLedgerSections(loadCloudLedger(), collectSyncItemValues(window.localStorage), now, ledgerSeedAt());
  saveCloudLedger(ledger);
}

function summarizeMerge(stats: { addedItems: number; removedItems: number; replacedItems: number; changedKeys: number; tombstones: number }): string {
  const parts: string[] = [];
  if (stats.addedItems) parts.push(`新增 ${stats.addedItems}`);
  if (stats.replacedItems) parts.push(`覆盖 ${stats.replacedItems}`);
  if (stats.removedItems) parts.push(`删除 ${stats.removedItems}`);
  const detail = parts.length ? parts.join("、") : "没有条目变化";
  return `${detail}；写入 ${stats.changedKeys} 个键，墓碑 ${stats.tombstones} 条`;
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
  reStampLedger(now);
  notifyConfigMerged({ appliedKeys: result.appliedKeys, removedKeys: result.removedKeys });
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
  return stableConfigFingerprint(window.localStorage);
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

type CloudSyncReason = "interval" | "change";

async function runCloudSyncPass(appVersion: string | null | undefined, reason: CloudSyncReason): Promise<string | null> {
  const settings = loadCloudSyncSettings();
  if (!settings.provider || !settings.autoSync) return null;

  const localChanged = refreshLocalConfigTimestamp();
  const seedLocalAt = loadCloudSyncState().localConfigAt;

  try {
    const remote = (await invoke<string | null>("cloud_sync_pull", { settings })) as string | null;
    const now = Date.now();
    if (!remote) {
      await cloudSyncPush(appVersion);
      patchState({ cloudConfigAt: now, lastError: null, ...(reason === "interval" ? { lastAutoSyncAt: now } : { lastChangeSyncAt: now }) });
      return "云端还没有配置文件，已把本机配置上传";
    }

    const payload = parseLanSyncPayload(JSON.parse(remote));
    const cloudAt = Date.parse(payload.exportedAt);
    const merged = applyCloudPayload(payload);

    if (seedLocalAt == null) {
      patchState({
        lastPullAt: now,
        cloudConfigAt: Number.isFinite(cloudAt) ? cloudAt : null,
        localConfigAt: Number.isFinite(cloudAt) ? cloudAt : now,
        lastConfigFingerprint: configFingerprint(),
        lastError: null,
        ...(reason === "interval" ? { lastAutoSyncAt: now } : { lastChangeSyncAt: now })
      });
      return `本地还没有时间戳，以云端配置时间初始化基线（已增量合并：${summarizeMerge(merged)}）`;
    }

    const localAt = merged.changed || localChanged ? now : seedLocalAt;
    patchState({
      lastPullAt: now,
      cloudConfigAt: Number.isFinite(cloudAt) ? cloudAt : null,
      localConfigAt: localAt,
      lastConfigFingerprint: configFingerprint(),
      lastError: null,
      ...(reason === "interval" ? { lastAutoSyncAt: now } : { lastChangeSyncAt: now })
    });

    if (localAt <= cloudAt) {
      return merged.changed ? `已合并云端配置：${summarizeMerge(merged)}` : "两端配置一致，无需同步";
    }

    await cloudSyncPush(appVersion);
    patchState({ cloudConfigAt: Date.now(), lastError: null });
    return merged.changed ? `已合并云端配置（${summarizeMerge(merged)}），本机较新已回传云端` : "本机配置更新，已上传云端";
  } catch (error) {
    patchState({ lastError: String((error as any)?.message ?? error) });
    return null;
  }
}

let passInFlight: Promise<string | null> | null = null;

function guardedCloudSyncPass(appVersion: string | null | undefined, reason: CloudSyncReason): Promise<string | null> {
  if (passInFlight) return passInFlight;
  const run = runCloudSyncPass(appVersion, reason).finally(() => {
    passInFlight = null;
  });
  passInFlight = run;
  return run;
}

export async function autoCloudSyncTick(appVersion?: string | null): Promise<string | null> {
  const settings = loadCloudSyncSettings();
  if (!settings.provider || !settings.autoSync) return null;
  refreshLocalConfigTimestamp();
  const state = loadCloudSyncState();
  const intervalMs = Math.max(1, Math.round(settings.intervalHours)) * 3_600_000;
  if (Date.now() - (state.lastAutoSyncAt ?? 0) < intervalMs) return null;
  return guardedCloudSyncPass(appVersion, "interval");
}

let storageHooksInstalled = false;
const writeListeners = new Set<() => void>();

function notifyConfigWrite(key: string | null | undefined) {
  if (!key || !CHANGE_TRIGGER_KEYS.includes(key)) return;
  for (const listener of writeListeners) listener();
}

function installStorageHooks() {
  if (storageHooksInstalled) return;
  if (typeof window === "undefined" || !window.localStorage) return;
  const proto = Object.getPrototypeOf(window.localStorage) as Storage | null;
  if (!proto || typeof proto.setItem !== "function") return;
  const originalSetItem = proto.setItem;
  const originalRemoveItem = proto.removeItem;
  proto.setItem = function (key: string, value: string) {
    originalSetItem.call(this, key, value);
    notifyConfigWrite(String(key));
  };
  proto.removeItem = function (key: string) {
    originalRemoveItem.call(this, key);
    notifyConfigWrite(String(key));
  };
  storageHooksInstalled = true;
}

export function startCloudSyncChangeWatcher(getAppVersion: () => string | null, onResult?: (message: string | null) => void): () => void {
  if (typeof window === "undefined" || !window.localStorage) return () => {};
  installStorageHooks();

  let timer: ReturnType<typeof setTimeout> | null = null;
  let rerunPending = false;
  let stopped = false;

  const schedule = () => {
    if (stopped || timer != null) return;
    timer = setTimeout(() => {
      timer = null;
      void flush();
    }, CHANGE_SYNC_DEBOUNCE_MS);
  };

  const flush = async () => {
    if (stopped) return;
    const settings = loadCloudSyncSettings();
    if (!settings.provider || !settings.autoSync) return;
    if (passInFlight) {
      rerunPending = true;
      return;
    }
    try {
      const message = await guardedCloudSyncPass(getAppVersion(), "change");
      if (message) onResult?.(message);
    } catch {
    } finally {
      if (rerunPending && !stopped) {
        rerunPending = false;
        schedule();
      }
    }
  };

  const listener = () => schedule();
  writeListeners.add(listener);

  return () => {
    stopped = true;
    writeListeners.delete(listener);
    if (timer != null) {
      clearTimeout(timer);
      timer = null;
    }
  };
}
