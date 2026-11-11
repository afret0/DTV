"use client";

export const LAN_SYNC_KIND = "dtv-lan-sync" as const;
export const LAN_SYNC_VERSION = 1 as const;

export const LAN_SYNC_KEYS = [
  "followedStreamers",
  "followFolders",
  "followListOrder",
  "dtv_custom_categories_v1",
  "danmu_block_keywords",
  "dtv_danmu_preferences_v1",
  "dtv_player_volume_v1",
  "dtv_player_volume_room_v1"
] as const;

type LanSyncKey = (typeof LAN_SYNC_KEYS)[number];

export const CLOUD_LOCAL_ONLY_KEYS = ["dtv_player_volume_v1", "dtv_player_volume_room_v1"] as const;

export const CLOUD_SYNC_KEYS: readonly LanSyncKey[] = LAN_SYNC_KEYS.filter(
  (key) => !CLOUD_LOCAL_ONLY_KEYS.includes(key as (typeof CLOUD_LOCAL_ONLY_KEYS)[number])
);

export const CONFIG_MERGED_EVENT = "dtv-config-merged";

export function notifyConfigMerged(detail: Record<string, number> = {}): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(CONFIG_MERGED_EVENT, { detail }));
}

type ConfigSource = {
  appVersion?: string | null;
  client: string;
};

export type PortableEntries = Record<string, string>;

export const CLOUD_REV_LEGACY = 1 as const;
export const CLOUD_REV_INCREMENTAL = 2 as const;

export type CloudItemStamp = { id: string; at: number };
export type CloudItemStamps = Record<string, CloudItemStamp[]>;

export interface LanSyncPayload {
  kind: typeof LAN_SYNC_KIND;
  version: typeof LAN_SYNC_VERSION;
  exportedAt: string;
  source: ConfigSource;
  entries: PortableEntries;
  rev?: number;
  itemAt?: CloudItemStamps;
  deletions?: CloudItemStamps;
}

export interface LanSyncSummary {
  followedStreamers: number;
  followFolders: number;
  followListOrder: number;
  customCategories: number;
  totalBytes?: number;
}

export interface LanSyncManifest {
  kind: typeof LAN_SYNC_KIND;
  version: typeof LAN_SYNC_VERSION;
  exportedAt: string;
  source: ConfigSource;
  summary: LanSyncSummary;
}

const isRecord = (value: unknown): value is Record<string, unknown> => {
  return !!value && typeof value === "object" && !Array.isArray(value);
};

function safeParseJsonArray<T>(raw: string | null | undefined): T[] {
  if (!raw || typeof raw !== "string") return [];
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as T[]) : [];
  } catch {
    return [];
  }
}

function safeParseJsonObject(raw: string | null | undefined): Record<string, unknown> | null {
  if (!raw || typeof raw !== "string") return null;
  try {
    const parsed = JSON.parse(raw);
    return isRecord(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function normalizeDanmuBlockKeywords(keywords: unknown): string[] {
  if (!Array.isArray(keywords)) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of keywords) {
    if (typeof raw !== "string") continue;
    const trimmed = raw.trim();
    if (!trimmed) continue;
    const normalized = trimmed.slice(0, 40);
    const dedupeKey = normalized.toLowerCase();
    if (seen.has(dedupeKey)) continue;
    seen.add(dedupeKey);
    out.push(normalized);
    if (out.length >= 40) break;
  }
  return out;
}

export function collectLanSyncEntries(storage: Storage): PortableEntries {
  const entries: PortableEntries = {};
  LAN_SYNC_KEYS.forEach((key) => {
    const value = storage.getItem(key);
    if (typeof value === "string") entries[key] = value;
  });
  return entries;
}

export function createLanSyncPayload(storage: Storage, source: Partial<ConfigSource> = {}): LanSyncPayload {
  return {
    kind: LAN_SYNC_KIND,
    version: LAN_SYNC_VERSION,
    exportedAt: new Date().toISOString(),
    source: {
      client: source.client ?? "desktop",
      appVersion: source.appVersion ?? null
    },
    entries: collectLanSyncEntries(storage)
  };
}

export function parseLanSyncManifest(raw: unknown): LanSyncManifest {
  if (!isRecord(raw)) throw new Error("Manifest format is invalid.");

  // Compatibility: some clients (e.g. older/mobile builds) omit kind/version fields.
  const kind = typeof raw.kind === "string" ? raw.kind : null;
  const version = typeof raw.version === "number" ? raw.version : null;
  if (kind && kind !== LAN_SYNC_KIND) throw new Error("This is not a DTV LAN sync export.");
  if (version != null && version !== LAN_SYNC_VERSION) throw new Error(`Unsupported sync version: ${String(raw.version)}.`);

  if (typeof raw.exportedAt !== "string" || Number.isNaN(Date.parse(raw.exportedAt))) throw new Error("Manifest exportedAt is invalid.");
  if (!isRecord(raw.source) || typeof raw.source.client !== "string" || !raw.source.client.trim()) throw new Error("Manifest source is invalid.");
  if (!isRecord(raw.summary)) throw new Error("Manifest summary is missing.");

  const summary = raw.summary;
  const num = (key: keyof LanSyncSummary) => (typeof summary[key] === "number" ? (summary[key] as number) : 0);

  return {
    kind: LAN_SYNC_KIND,
    version: LAN_SYNC_VERSION,
    exportedAt: raw.exportedAt,
    source: {
      client: raw.source.client,
      appVersion: typeof raw.source.appVersion === "string" ? raw.source.appVersion : null
    },
    summary: {
      followedStreamers: num("followedStreamers"),
      followFolders: num("followFolders"),
      followListOrder: num("followListOrder"),
      customCategories: num("customCategories"),
      totalBytes: typeof summary.totalBytes === "number" ? (summary.totalBytes as number) : undefined
    }
  };
}

export function parseLanSyncPayload(raw: unknown): LanSyncPayload {
  if (!isRecord(raw)) throw new Error("Payload format is invalid.");

  // Compatibility: some clients (e.g. older/mobile builds) omit kind/version fields.
  const kind = typeof raw.kind === "string" ? raw.kind : null;
  const version = typeof raw.version === "number" ? raw.version : null;
  if (kind && kind !== LAN_SYNC_KIND) throw new Error("This is not a DTV LAN sync export.");
  if (version != null && version !== LAN_SYNC_VERSION) throw new Error(`Unsupported sync version: ${String(raw.version)}.`);

  if (typeof raw.exportedAt !== "string" || Number.isNaN(Date.parse(raw.exportedAt))) throw new Error("Payload exportedAt is invalid.");
  if (!isRecord(raw.source) || typeof raw.source.client !== "string" || !raw.source.client.trim()) throw new Error("Payload source is invalid.");
  if (!isRecord(raw.entries)) throw new Error("Payload entries are missing.");

  const entries: PortableEntries = {};
  Object.entries(raw.entries).forEach(([key, value]) => {
    if (!LAN_SYNC_KEYS.includes(key as LanSyncKey)) return;
    if (typeof value !== "string") return;
    entries[key] = value;
  });

  const itemAt = parseItemStamps(raw.itemAt);
  const deletions = parseItemStamps(raw.deletions);

  return {
    kind: LAN_SYNC_KIND,
    version: LAN_SYNC_VERSION,
    exportedAt: raw.exportedAt,
    source: {
      client: raw.source.client,
      appVersion: typeof raw.source.appVersion === "string" ? raw.source.appVersion : null
    },
    entries,
    rev: typeof raw.rev === "number" && Number.isFinite(raw.rev) ? raw.rev : CLOUD_REV_LEGACY,
    itemAt,
    deletions
  };
}

function parseItemStamps(raw: unknown): CloudItemStamps {
  const out: CloudItemStamps = {};
  if (!isRecord(raw)) return out;
  for (const [key, list] of Object.entries(raw)) {
    if (!CLOUD_SYNC_KEYS.includes(key as LanSyncKey) || !Array.isArray(list)) continue;
    const stamps: CloudItemStamp[] = [];
    for (const item of list) {
      if (!isRecord(item)) continue;
      const id = typeof item.id === "string" ? item.id : "";
      const at = typeof item.at === "number" && Number.isFinite(item.at) ? Math.max(0, item.at) : 0;
      stamps.push({ id, at });
    }
    out[key] = stamps;
  }
  return out;
}

type FollowPlatform = "DOUYU" | "DOUYIN" | "HUYA" | "BILIBILI";

type FollowedStreamer = {
  id: string;
  platform: FollowPlatform;
  nickname: string;
  avatarUrl: string;
  roomTitle?: string;
  currentRoomId: string;
  liveStatus: "UNKNOWN" | "LIVE" | "OFFLINE";
};

type FollowFolder = {
  id: string;
  name: string;
  streamerIds: string[];
  expanded?: boolean;
};

type FollowListItem = { type: "folder"; data: FollowFolder } | { type: "streamer"; data: FollowedStreamer };

function normalizeStreamerKey(streamerKey: string) {
  const [rawPlatform, rawId] = String(streamerKey || "").split(":");
  const platform = String(rawPlatform || "").toUpperCase();
  const id = String(rawId || "");
  return { platform, id, key: `${platform}:${id}` };
}

function isKnownPlatform(platform: string): platform is FollowPlatform {
  return platform === "DOUYU" || platform === "DOUYIN" || platform === "HUYA" || platform === "BILIBILI";
}

function ensureUuid(existing: Set<string>, seed?: string): string {
  const raw = String(seed || "").trim();
  if (raw && !existing.has(raw)) return raw;
  const gen = () => {
    const c = (globalThis as any).crypto;
    if (c?.randomUUID) return c.randomUUID();
    return `id-${Date.now().toString(16)}-${Math.random().toString(16).slice(2)}`;
  };
  for (let i = 0; i < 8; i += 1) {
    const next = gen();
    if (!existing.has(next)) return next;
  }
  return `${raw || "id"}-${Date.now().toString(16)}`;
}

function uniqPreserveOrder(values: string[]) {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const v of values) {
    if (seen.has(v)) continue;
    seen.add(v);
    out.push(v);
  }
  return out;
}

export type LanSyncImportResult = {
  addedStreamers: number;
  addedFolderCount: number;
  addedFolderItems: number;
  addedCustomCategories: number;
  addedDanmuBlockKeywords: number;
  appliedDanmuPreferences: boolean;
};

export function applyIncrementalLanSyncImport(storage: Storage, remoteEntries: PortableEntries): LanSyncImportResult {
  const remoteFoldersFromKey = safeParseJsonArray<FollowFolder>(remoteEntries["followFolders"]);
  const remoteStreamersFromKey = safeParseJsonArray<FollowedStreamer>(remoteEntries["followedStreamers"]);
  const remoteOrderFromKey = safeParseJsonArray<FollowListItem>(remoteEntries["followListOrder"]);
  const remoteCustomCategoriesFromKey = safeParseJsonArray<{ key: string }>(remoteEntries["dtv_custom_categories_v1"]);
  const remoteDanmuBlockKeywordsFromKey = safeParseJsonArray<string>(remoteEntries["danmu_block_keywords"]);
  const remoteDanmuPreferencesFromKey = safeParseJsonObject(remoteEntries["dtv_danmu_preferences_v1"]);

  const localFollowed = safeParseJsonArray<FollowedStreamer>(storage.getItem("followedStreamers"));
  const localFolders = safeParseJsonArray<FollowFolder>(storage.getItem("followFolders"));
  const localOrder = safeParseJsonArray<FollowListItem>(storage.getItem("followListOrder"));
  const localCustomCategories = safeParseJsonArray<{ key: string }>(storage.getItem("dtv_custom_categories_v1"));
  const localDanmuBlockKeywords = safeParseJsonArray<string>(storage.getItem("danmu_block_keywords"));
  const localDanmuPreferencesRaw = storage.getItem("dtv_danmu_preferences_v1");
  const localDanmuPreferences = safeParseJsonObject(localDanmuPreferencesRaw);

  let addedStreamers = 0;
  let addedFolderCount = 0;
  let addedFolderItems = 0;
  let addedCustomCategories = 0;
  let addedDanmuBlockKeywords = 0;
  let appliedDanmuPreferences = false;

  const streamerByKey = new Map<string, FollowedStreamer>();
  for (const s of localFollowed) {
    if (!s?.platform || !s?.id) continue;
    streamerByKey.set(`${s.platform}:${s.id}`, s);
  }

  const addStreamerIfMissing = (platform: string, id: string, streamer?: Partial<FollowedStreamer>) => {
    const p = String(platform || "").toUpperCase();
    if (!isKnownPlatform(p)) return;
    const sid = String(id || "");
    if (!sid) return;
    const key = `${p}:${sid}`;
    if (streamerByKey.has(key)) return;
    streamerByKey.set(key, {
      id: sid,
      platform: p,
      nickname: streamer?.nickname ?? sid,
      avatarUrl: streamer?.avatarUrl ?? "",
      roomTitle: streamer?.roomTitle ?? "",
      currentRoomId: streamer?.currentRoomId ?? sid,
      liveStatus: streamer?.liveStatus ?? "UNKNOWN"
    });
    addedStreamers += 1;
  };

  const remoteFolders = [...remoteFoldersFromKey];

  // Merge remote followed streamers if present.
  for (const rs of remoteStreamersFromKey) {
    if (!rs?.platform || !rs?.id) continue;
    addStreamerIfMissing(String(rs.platform), String(rs.id), rs);
  }

  // Ensure streamers referenced by remote order exist.
  for (const item of remoteOrderFromKey) {
    if (!item || typeof item !== "object") continue;
    if (item.type === "streamer") {
      const s = (item as any).data as Partial<FollowedStreamer> | undefined;
      const platform = String(s?.platform || "");
      const id = String(s?.id || "");
      if (!platform || !id) continue;
      addStreamerIfMissing(platform, id, s);
      continue;
    }
    if (item.type === "folder") {
      const f = (item as any).data as Partial<FollowFolder> | undefined;
      for (const rawKey of (f?.streamerIds ?? []) as any[]) {
        const norm = normalizeStreamerKey(String(rawKey));
        if (!norm.id || !isKnownPlatform(norm.platform)) continue;
        addStreamerIfMissing(norm.platform, norm.id);
      }
    }
  }

  // Ensure streamers referenced by folders/order exist (align with FollowProvider)
  for (const folder of remoteFolders) {
    for (const rawKey of folder?.streamerIds ?? []) {
      const norm = normalizeStreamerKey(rawKey);
      if (!norm.id || !isKnownPlatform(norm.platform)) continue;
      addStreamerIfMissing(norm.platform, norm.id);
    }
  }

  const existingFolderIds = new Set(localFolders.map((f) => String(f?.id || "")).filter(Boolean));
  const folderByName = new Map<string, FollowFolder>();
  for (const f of localFolders) {
    const name = String(f?.name || "").trim();
    if (!name || !f?.id) continue;
    folderByName.set(name.toLowerCase(), f);
  }

  const mergedFolders: FollowFolder[] = [...localFolders.map((f) => ({ ...f, streamerIds: Array.isArray(f.streamerIds) ? [...f.streamerIds] : [] }))];
  const mergedFolderById = new Map<string, FollowFolder>();
  mergedFolders.forEach((f) => {
    if (f?.id) mergedFolderById.set(f.id, f);
  });

  for (const rf of remoteFolders) {
    const name = String(rf?.name || "").trim();
    if (!name) continue;
    const match = folderByName.get(name.toLowerCase());
    const nextStreamerIds = uniqPreserveOrder(
      (Array.isArray(rf?.streamerIds) ? rf.streamerIds : [])
        .map((x) => normalizeStreamerKey(String(x)).key)
        .filter((k) => {
          const norm = normalizeStreamerKey(k);
          return !!norm.id && isKnownPlatform(norm.platform);
        })
    );

    if (match && match.id) {
      const target = mergedFolderById.get(match.id);
      if (!target) continue;
      const before = target.streamerIds.length;
      const existing = new Set(target.streamerIds.map((x) => normalizeStreamerKey(String(x)).key));
      for (const k of nextStreamerIds) {
        if (existing.has(k)) continue;
        existing.add(k);
        target.streamerIds.push(k);
      }
      const delta = target.streamerIds.length - before;
      if (delta > 0) addedFolderItems += delta;
      continue;
    }

    const id = ensureUuid(existingFolderIds, rf?.id);
    existingFolderIds.add(id);
    const folder: FollowFolder = {
      id,
      name,
      streamerIds: nextStreamerIds,
      expanded: true
    };
    mergedFolders.push(folder);
    mergedFolderById.set(id, folder);
    folderByName.set(name.toLowerCase(), folder);
    addedFolderCount += 1;
    addedFolderItems += nextStreamerIds.length;
  }

  const mergedStreamers = Array.from(streamerByKey.values());

  // Rebuild list order: keep local order, append newly added folders/streamers.
  const folderedKeys = new Set<string>();
  for (const f of mergedFolders) {
    for (const rawKey of f.streamerIds ?? []) {
      folderedKeys.add(normalizeStreamerKey(String(rawKey)).key);
    }
  }

  const localFolderOrder = (Array.isArray(localOrder) ? localOrder : []).filter((x): x is Extract<FollowListItem, { type: "folder" }> => x?.type === "folder");
  const localStreamerOrder = (Array.isArray(localOrder) ? localOrder : []).filter((x): x is Extract<FollowListItem, { type: "streamer" }> => x?.type === "streamer");

  const folderIdsInOrder = new Set(localFolderOrder.map((x) => String(x?.data?.id || "")).filter(Boolean));
  const updatedFolderItems: FollowListItem[] = [];
  for (const item of localFolderOrder) {
    const id = String(item?.data?.id || "");
    const merged = id ? mergedFolderById.get(id) : null;
    if (!merged) continue;
    updatedFolderItems.push({ type: "folder", data: merged });
  }
  for (const f of mergedFolders) {
    if (!f?.id || folderIdsInOrder.has(f.id)) continue;
    updatedFolderItems.push({ type: "folder", data: f });
  }

  const streamerKeysInOrder = new Set<string>();
  const updatedStreamerItems: FollowListItem[] = [];
  for (const item of localStreamerOrder) {
    const s = item.data;
    if (!s?.platform || !s?.id) continue;
    const key = normalizeStreamerKey(`${s.platform}:${s.id}`).key;
    if (folderedKeys.has(key) || streamerKeysInOrder.has(key)) continue;
    streamerKeysInOrder.add(key);
    const merged = streamerByKey.get(`${String(s.platform).toUpperCase()}:${String(s.id)}`);
    updatedStreamerItems.push({ type: "streamer", data: merged ?? s });
  }
  for (const s of mergedStreamers) {
    const key = normalizeStreamerKey(`${s.platform}:${s.id}`).key;
    if (folderedKeys.has(key) || streamerKeysInOrder.has(key)) continue;
    streamerKeysInOrder.add(key);
    updatedStreamerItems.push({ type: "streamer", data: s });
  }

  const mergedOrder: FollowListItem[] = [...updatedFolderItems, ...updatedStreamerItems];

  // Merge custom categories by key (incremental).
  const localCategories = Array.isArray(localCustomCategories) ? localCustomCategories : [];
  const remoteCategories = Array.isArray(remoteCustomCategoriesFromKey) ? remoteCustomCategoriesFromKey : [];
  const categoryByKey = new Map<string, any>();
  const mergedCategories: any[] = [];
  for (const c of localCategories) {
    const key = String((c as any)?.key || "").trim();
    if (!key || categoryByKey.has(key)) continue;
    categoryByKey.set(key, c);
    mergedCategories.push(c);
  }
  for (const c of remoteCategories) {
    const key = String((c as any)?.key || "").trim();
    if (!key || categoryByKey.has(key)) continue;
    categoryByKey.set(key, c);
    mergedCategories.push(c);
    addedCustomCategories += 1;
  }

  // Merge danmu block keywords (incremental).
  const localKeywords = normalizeDanmuBlockKeywords(localDanmuBlockKeywords);
  const remoteKeywords = normalizeDanmuBlockKeywords(remoteDanmuBlockKeywordsFromKey);
  const mergedKeywords = normalizeDanmuBlockKeywords([...localKeywords, ...remoteKeywords]);
  addedDanmuBlockKeywords = Math.max(0, mergedKeywords.length - localKeywords.length);

  // Apply danmu preferences only when local missing/invalid (incremental).
  const canApplyRemotePreferences = !localDanmuPreferences && !!remoteDanmuPreferencesFromKey;

  storage.setItem("followedStreamers", JSON.stringify(mergedStreamers));
  storage.setItem("followFolders", JSON.stringify(mergedFolders));
  storage.setItem("followListOrder", JSON.stringify(mergedOrder));
  if (mergedCategories.length) storage.setItem("dtv_custom_categories_v1", JSON.stringify(mergedCategories));
  if (mergedKeywords.length) storage.setItem("danmu_block_keywords", JSON.stringify(mergedKeywords));
  if (canApplyRemotePreferences) {
    storage.setItem("dtv_danmu_preferences_v1", JSON.stringify(remoteDanmuPreferencesFromKey));
    appliedDanmuPreferences = true;
  }

  return {
    addedStreamers,
    addedFolderCount,
    addedFolderItems,
    addedCustomCategories,
    addedDanmuBlockKeywords,
    appliedDanmuPreferences
  };
}

export type LanSyncOverwriteResult = {
  appliedKeys: number;
  removedKeys: number;
};

export function applyFullLanSyncOverwrite(storage: Storage, remoteEntries: PortableEntries): LanSyncOverwriteResult {
  let appliedKeys = 0;
  let removedKeys = 0;
  for (const key of LAN_SYNC_KEYS) {
    const remote = remoteEntries[key];
    if (typeof remote === "string") {
      if (storage.getItem(key) !== remote) {
        storage.setItem(key, remote);
        appliedKeys += 1;
      }
    } else if (storage.getItem(key) !== null) {
      storage.removeItem(key);
      removedKeys += 1;
    }
  }
  return { appliedKeys, removedKeys };
}

export const CLOUD_TOMBSTONE_TTL_MS = 180 * 24 * 3600 * 1000;

const STREAMER_SIG_FIELDS = ["id", "platform", "nickname", "avatarUrl", "currentRoomId"] as const;
const WHOLE_VALUE_SIG = "__whole__";
const DANMU_PREFERENCES_KEY = "dtv_danmu_preferences_v1";
const COLLECTION_KEYS = ["followedStreamers", "followFolders", "dtv_custom_categories_v1", "danmu_block_keywords"] as const;

const stringify = (value: unknown): string => {
  if (value === undefined) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value) ?? "";
  } catch {
    return "";
  }
};

function stableSignature(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value !== "object") return String(value);
  if (Array.isArray(value)) return `[${value.map(stableSignature).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${Object.keys(record)
    .sort()
    .map((k) => `${k}:${stableSignature(record[k])}`)
    .join(",")}}`;
}

function itemSignature(storageKey: string, itemId: string, value: unknown): string {
  if (storageKey === "followedStreamers") {
    const record = (isRecord(value) ? value : {}) as Partial<FollowedStreamer>;
    return stableSignature(STREAMER_SIG_FIELDS.map((field) => stringify(record[field])));
  }
  if (storageKey === "followFolders") {
    const record = (isRecord(value) ? value : {}) as Partial<FollowFolder>;
    const ids = uniqPreserveOrder((Array.isArray(record.streamerIds) ? record.streamerIds : []).map((x) => normalizeStreamerKey(String(x)).key));
    return stableSignature({ name: String(record.name ?? ""), ids });
  }
  if (storageKey === DANMU_PREFERENCES_KEY && itemId.includes(".")) return stableSignature(value);
  if (storageKey === "followListOrder") {
    return stringify(
      safeParseJsonArray<FollowListItem>(stringify(value)).map((item) =>
        item?.type === "folder" ? `folder:${String(item?.data?.id || "")}` : `streamer:${normalizeStreamerKey(`${item?.data?.platform}:${item?.data?.id}`).key}`
      )
    );
  }
  return stableSignature(value);
}

export function collectSyncItemValues(storage: Storage): Record<string, Record<string, unknown>> {
  return collectSyncItemValuesFrom((key) => storage.getItem(key));
}

export function collectSyncItemValuesFromEntries(entries: PortableEntries): Record<string, Record<string, unknown>> {
  return collectSyncItemValuesFrom((key) => entries[key] ?? null);
}

export function stableConfigFingerprint(storage: Storage = localStorage): string {
  const values = collectSyncItemValues(storage);
  const parts: string[] = [];
  for (const key of CLOUD_SYNC_KEYS) {
    const pairs = Object.entries(values[key] ?? {}).sort((left, right) => left[0].localeCompare(right[0]));
    for (const [id, value] of pairs) parts.push(`${key}/${id}=${itemSignature(key, id, value)}`);
  }
  const text = parts.join("\u0000");
  let hash = 5381;
  for (let i = 0; i < text.length; i += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(i)) | 0;
  }
  return `${text.length}:${hash}`;
}

function collectSyncItemValuesFrom(getItem: (key: string) => string | null): Record<string, Record<string, unknown>> {
  const values: Record<string, Record<string, unknown>> = {};
  for (const key of CLOUD_SYNC_KEYS) {
    const raw = getItem(key);
    if (raw == null) continue;
    const bucket: Record<string, unknown> = {};
    if (key === "followedStreamers") {
      for (const s of safeParseJsonArray<FollowedStreamer>(raw)) {
        if (!s?.id) continue;
        const platform = normalizeStreamerKey(`${String(s.platform || "").toUpperCase()}:${s.id}`).platform;
        if (!isKnownPlatform(platform)) continue;
        bucket[normalizeStreamerKey(`${platform}:${s.id}`).key] = s;
      }
    } else if (key === "followFolders") {
      for (const f of safeParseJsonArray<FollowFolder>(raw)) {
        if (!f?.id) continue;
        bucket[String(f.id)] = f;
      }
    } else if (key === "dtv_custom_categories_v1") {
      for (const c of safeParseJsonArray<{ key?: unknown }>(raw)) {
        const id = String((c as any)?.key ?? "").trim();
        if (!id) continue;
        bucket[id] = c;
      }
    } else if (key === "danmu_block_keywords") {
      for (const kw of normalizeDanmuBlockKeywords(safeParseJsonArray<string>(raw))) bucket[kw] = kw;
    } else if (key === DANMU_PREFERENCES_KEY) {
      const prefs = safeParseJsonObject(raw);
      if (!prefs) continue;
      for (const [field, fieldValue] of Object.entries(prefs)) {
        if (field === "settings" && isRecord(fieldValue)) {
          for (const [name, nested] of Object.entries(fieldValue)) bucket[`settings.${name}`] = nested;
          continue;
        }
        bucket[field] = fieldValue;
      }
    } else {
      bucket[WHOLE_VALUE_SIG] = raw;
    }
    if (Object.keys(bucket).length) values[key] = bucket;
  }
  return values;
}

export type CloudLedgerItem = { at: number; sig: string };
export type CloudLedger = {
  items: Record<string, Record<string, CloudLedgerItem>>;
  deletions: Record<string, Record<string, number>>;
};

function emptyCloudLedger(): CloudLedger {
  return { items: {}, deletions: {} };
}

export function parseCloudLedger(raw: unknown): CloudLedger {
  const ledger = emptyCloudLedger();
  if (!isRecord(raw)) return ledger;
  if (isRecord(raw.items)) {
    for (const [key, bucket] of Object.entries(raw.items)) {
      if (!CLOUD_SYNC_KEYS.includes(key as LanSyncKey) || !isRecord(bucket)) continue;
      const items: Record<string, CloudLedgerItem> = {};
      for (const [id, value] of Object.entries(bucket)) {
        if (typeof value === "number") {
          items[id] = { at: value, sig: "" };
          continue;
        }
        if (!isRecord(value)) continue;
        items[id] = { at: typeof value.at === "number" && Number.isFinite(value.at) ? value.at : 0, sig: typeof value.sig === "string" ? value.sig : "" };
      }
      ledger.items[key] = items;
    }
  }
  if (isRecord(raw.deletions)) {
    for (const [key, bucket] of Object.entries(raw.deletions)) {
      if (!CLOUD_SYNC_KEYS.includes(key as LanSyncKey) || !isRecord(bucket)) continue;
      const deletions: Record<string, number> = {};
      for (const [id, at] of Object.entries(bucket)) {
        deletions[id] = typeof at === "number" && Number.isFinite(at) ? at : 0;
      }
      ledger.deletions[key] = deletions;
    }
  }
  return ledger;
}

function stampsToMap(stamps: CloudItemStamp[] | undefined): Map<string, number> {
  const map = new Map<string, number>();
  for (const stamp of stamps ?? []) {
    if (!stamp?.id) continue;
    const at = typeof stamp.at === "number" && Number.isFinite(stamp.at) ? stamp.at : 0;
    const prev = map.get(stamp.id);
    map.set(stamp.id, prev == null ? at : Math.max(prev, at));
  }
  return map;
}

function pruneTombstones(deletions: Record<string, number>, now: number) {
  const kept: Record<string, number> = {};
  for (const [id, at] of Object.entries(deletions)) {
    if (now - at > CLOUD_TOMBSTONE_TTL_MS) continue;
    kept[id] = at;
  }
  return kept;
}

function stampLedger(
  ledger: CloudLedger,
  localValues: Record<string, Record<string, unknown>>,
  now: number,
  seedAt: number | null
): CloudLedger {
  const next: CloudLedger = { items: {}, deletions: {} };
  for (const key of CLOUD_SYNC_KEYS) {
    const bucket = localValues[key] ?? {};
    const prevItems = ledger.items[key] ?? {};
    const items: Record<string, CloudLedgerItem> = {};
    for (const [id, value] of Object.entries(bucket)) {
      const sig = itemSignature(key, id, value);
      const prev = prevItems[id];
      items[id] = prev && prev.sig === sig ? { at: prev.at, sig } : { at: prev ? now : (seedAt ?? now), sig };
    }
    next.items[key] = items;

    const prevDeletions = ledger.deletions[key] ?? {};
    const deletions: Record<string, number> = { ...prevDeletions };
    for (const id of Object.keys(prevItems)) {
      if (id in bucket) continue;
      if (!(id in deletions)) deletions[id] = now;
    }
    for (const [id, at] of Object.entries(deletions)) {
      const item = items[id];
      if (item && item.at > at) delete deletions[id];
    }
    next.deletions[key] = pruneTombstones(deletions, now);
  }
  return next;
}

export function buildCloudLedgerSections(ledger: CloudLedger, localValues: Record<string, Record<string, unknown>>, now: number, seedAt: number | null) {
  const next = stampLedger(ledger, localValues, now, seedAt);
  const itemAt: CloudItemStamps = {};
  const deletions: CloudItemStamps = {};
  for (const key of CLOUD_SYNC_KEYS) {
    const stamps = Object.entries(next.items[key] ?? {}).map(([id, item]) => ({ id, at: item.at }));
    if (stamps.length) itemAt[key] = stamps;
    const tombstones = Object.entries(next.deletions[key] ?? {}).map(([id, at]) => ({ id, at }));
    if (tombstones.length) deletions[key] = tombstones;
  }
  return { ledger: next, itemAt, deletions };
}

function rebuildFollowOrder(order: FollowListItem[], folders: FollowFolder[], streamers: FollowedStreamer[]): FollowListItem[] {
  const folderedKeys = new Set<string>(folders.flatMap((f) => (f?.streamerIds ?? []).map((x) => normalizeStreamerKey(String(x)).key)));
  const streamerByKey = new Map<string, FollowedStreamer>(streamers.map((s) => [normalizeStreamerKey(`${s.platform}:${s.id}`).key, s]));
  const next: FollowListItem[] = [];
  const seen = new Set<string>();

  for (const item of order) {
    if (item?.type === "folder") {
      const id = String(item?.data?.id || "");
      if (!id || seen.has(`folder:${id}`)) continue;
      const folder = folders.find((f) => String(f?.id || "") === id);
      if (!folder) continue;
      seen.add(`folder:${id}`);
      next.push({ type: "folder", data: folder });
      continue;
    }
    if (item?.type !== "streamer") continue;
    const key = normalizeStreamerKey(`${item?.data?.platform}:${item?.data?.id}`).key;
    if (!key || seen.has(`streamer:${key}`) || folderedKeys.has(key)) continue;
    const streamer = streamerByKey.get(key);
    if (!streamer) continue;
    seen.add(`streamer:${key}`);
    next.push({ type: "streamer", data: streamer });
  }

  for (const folder of folders) {
    const id = String(folder?.id || "");
    if (!id || seen.has(`folder:${id}`)) continue;
    seen.add(`folder:${id}`);
    next.push({ type: "folder", data: folder });
  }
  for (const streamer of streamers) {
    const key = normalizeStreamerKey(`${streamer.platform}:${streamer.id}`).key;
    if (seen.has(`streamer:${key}`) || folderedKeys.has(key)) continue;
    seen.add(`streamer:${key}`);
    next.push({ type: "streamer", data: streamer });
  }
  return next;
}

const WHOLE_VALUE_KEYS = ["followListOrder", "dtv_player_volume_v1", "dtv_player_volume_room_v1"] as const;

function serializeMergedKey(
  key: string,
  merged: Record<string, unknown>,
  mergedByKey: Record<string, Record<string, unknown>>,
  previousRaw: string | null
): string | null {
  const isEmpty = !Object.keys(merged).length;
  if (key === DANMU_PREFERENCES_KEY) {
    if (isEmpty) return previousRaw == null ? null : "{}";
    const prefs: Record<string, unknown> = {};
    const settings: Record<string, unknown> = {};
    for (const [id, value] of Object.entries(merged)) {
      if (id.startsWith("settings.")) settings[id.slice("settings.".length)] = value;
      else prefs[id] = value;
    }
    if (Object.keys(settings).length) prefs.settings = settings;
    return JSON.stringify(prefs);
  }
  if (key === "followListOrder") {
    const rawOrder = typeof merged[WHOLE_VALUE_SIG] === "string" ? (merged[WHOLE_VALUE_SIG] as string) : null;
    const folders = Object.values(mergedByKey["followFolders"] ?? {}).filter((f): f is FollowFolder => !!f) as FollowFolder[];
    const streamers = Object.values(mergedByKey["followedStreamers"] ?? {}).filter((s): s is FollowedStreamer => !!s) as FollowedStreamer[];
    const rebuilt = rebuildFollowOrder(safeParseJsonArray<FollowListItem>(rawOrder), folders, streamers);
    if (!rebuilt.length && previousRaw == null) return null;
    return JSON.stringify(rebuilt);
  }
  if (WHOLE_VALUE_KEYS.includes(key as (typeof WHOLE_VALUE_KEYS)[number])) {
    const whole = merged[WHOLE_VALUE_SIG];
    return typeof whole === "string" && whole.length ? whole : null;
  }
  if (isEmpty) return previousRaw == null ? null : "[]";
  if (key === "danmu_block_keywords") return JSON.stringify(normalizeDanmuBlockKeywords(Object.keys(merged)));
  return JSON.stringify(Object.values(merged).filter((v) => v !== undefined));
}

export type CloudReconcileResult = {
  changed: boolean;
  ledger: CloudLedger;
  stats: { addedItems: number; removedItems: number; replacedItems: number; changedKeys: number; tombstones: number };
};

export function reconcileCloudSync(
  storage: Storage,
  payload: LanSyncPayload,
  ledger: CloudLedger,
  now: number,
  seedAt: number | null = null
): CloudReconcileResult {
  const localValues = collectSyncItemValues(storage);
  const remoteValues = collectSyncItemValuesFromEntries(payload.entries);

  const nextLedger: CloudLedger = { items: {}, deletions: {} };
  const mergedByKey: Record<string, Record<string, unknown>> = {};
  let addedItems = 0;
  let removedItems = 0;
  let replacedItems = 0;
  let tombstones = 0;

  for (const key of CLOUD_SYNC_KEYS) {
    const local = localValues[key] ?? {};
    const remote = remoteValues[key] ?? {};
    const remoteAt = stampsToMap(payload.itemAt?.[key]);
    const prevItems = ledger.items[key] ?? {};

    const mergedDeletions: Record<string, number> = { ...(ledger.deletions[key] ?? {}) };
    for (const stamp of payload.deletions?.[key] ?? []) {
      if (!stamp?.id) continue;
      mergedDeletions[stamp.id] = Math.max(mergedDeletions[stamp.id] ?? 0, typeof stamp.at === "number" && Number.isFinite(stamp.at) ? stamp.at : 0);
    }

    const mergedItems: Record<string, CloudLedgerItem> = {};
    const merged: Record<string, unknown> = {};

    for (const id of new Set([...Object.keys(local), ...Object.keys(remote), ...Object.keys(prevItems)])) {
      const hasLocal = id in local;
      const hasRemote = id in remote;
      const prevAt = prevItems[id]?.at ?? 0;
      const localAt = hasLocal ? (prevAt > 0 ? prevAt : (seedAt ?? now)) : 0;
      const remoteItemAt = remoteAt.get(id) ?? 0;
      const tombstoneAt = mergedDeletions[id] ?? 0;

      if (!hasLocal && !hasRemote) continue;

      const blocked = tombstoneAt > 0 && (hasLocal ? localAt <= tombstoneAt : true) && (!hasRemote || remoteItemAt <= tombstoneAt);
      if (blocked) {
        if (hasLocal) removedItems += 1;
        mergedDeletions[id] = tombstoneAt;
        continue;
      }

      const winnerIsRemote = hasRemote && (!hasLocal || remoteItemAt > localAt);
      const value = winnerIsRemote ? remote[id] : local[id];
      const at = Math.max(winnerIsRemote ? remoteItemAt : 0, localAt);
      merged[id] = value;
      mergedItems[id] = { at, sig: itemSignature(key, id, value) };
      delete mergedDeletions[id];
      if (winnerIsRemote) {
        if (hasLocal) replacedItems += 1;
        else addedItems += 1;
      }
    }

    for (const id of Object.keys(prevItems)) {
      if (id in mergedItems || id in mergedDeletions) continue;
      mergedDeletions[id] = now;
      removedItems += 1;
    }

    nextLedger.items[key] = mergedItems;
    nextLedger.deletions[key] = pruneTombstones(mergedDeletions, now);
    tombstones += Object.keys(nextLedger.deletions[key]).length;
    mergedByKey[key] = merged;
  }

  let changedKeys = 0;
  for (const key of CLOUD_SYNC_KEYS) {
    const previousRaw = storage.getItem(key);
    const nextRaw = serializeMergedKey(key, mergedByKey[key] ?? {}, mergedByKey, previousRaw);
    if (nextRaw == null) {
      if (previousRaw != null) {
        storage.removeItem(key);
        changedKeys += 1;
      }
      continue;
    }
    if (previousRaw !== nextRaw) {
      storage.setItem(key, nextRaw);
      changedKeys += 1;
    }
  }

  const changed = addedItems + removedItems + replacedItems + changedKeys > 0;

  if (changed) notifyConfigMerged({ addedItems, removedItems, replacedItems, changedKeys });

  return {
    changed,
    ledger: nextLedger,
    stats: { addedItems, removedItems, replacedItems, changedKeys, tombstones }
  };
}
