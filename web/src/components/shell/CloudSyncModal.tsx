"use client";

import React, { useCallback, useEffect, useState } from "react";
import { CloudDownload, CloudUpload, PlugZap } from "lucide-react";

import styles from "./Navbar.module.css";
import {
  cloudSyncPull,
  cloudSyncPush,
  cloudSyncTest,
  loadCloudSyncSettings,
  loadCloudSyncState,
  saveCloudSyncSettings,
  SYNC_INTERVAL_OPTIONS,
  type CloudSyncSettings,
  type CloudSyncState
} from "@/services/cloudSync";

function formatTime(ts: number | null): string {
  if (!ts) return "从未";
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getMonth() + 1}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 云同步配置面板：可独立成弹窗，也可嵌入「数据同步」弹窗作为页签。 */
export function CloudSyncPanel({ appVersion }: { appVersion?: string }) {
  const [settings, setSettings] = useState<CloudSyncSettings>(() => loadCloudSyncSettings());
  const [state, setState] = useState<CloudSyncState>(() => loadCloudSyncState());
  const [busy, setBusy] = useState<"" | "test" | "push" | "pull">("");
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 每次挂载（弹窗打开/切到云同步页签）都重新读一遍本地配置与状态。
  useEffect(() => {
    setSettings(loadCloudSyncSettings());
    setState(loadCloudSyncState());
    setMessage(null);
    setError(null);
    setBusy("");
  }, []);

  const patch = useCallback((next: Partial<CloudSyncSettings>) => {
    setSettings((prev) => {
      const merged = { ...prev, ...next };
      saveCloudSyncSettings(merged);
      return merged;
    });
  }, []);

  const patchOss = useCallback(
    (next: Partial<CloudSyncSettings["oss"]>) => {
      setSettings((prev) => {
        const merged = { ...prev, oss: { ...prev.oss, ...next } };
        saveCloudSyncSettings(merged);
        return merged;
      });
    },
    []
  );

  const patchWebdav = useCallback(
    (next: Partial<CloudSyncSettings["webdav"]>) => {
      setSettings((prev) => {
        const merged = { ...prev, webdav: { ...prev.webdav, ...next } };
        saveCloudSyncSettings(merged);
        return merged;
      });
    },
    []
  );

  const run = useCallback(
    async (kind: "test" | "push" | "pull") => {
      setBusy(kind);
      setMessage(null);
      setError(null);
      try {
        const result =
          kind === "test"
            ? await cloudSyncTest()
            : kind === "push"
              ? await cloudSyncPush(appVersion)
              : await cloudSyncPull(appVersion);
        setMessage(result);
        setState(loadCloudSyncState());
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        setError(msg);
        setState(loadCloudSyncState());
      } finally {
        setBusy("");
      }
    },
    [appVersion]
  );

  return (
    <>
      <div className={styles.overlayBody} style={{ padding: 0 }}>
              <div className={styles.syncField}>
                <span className={styles.syncLabel}>同步方式</span>
                <div className={styles.syncTabs}>
                  <button
                    type="button"
                    className={`${styles.syncTab} ${styles.syncBtnGlass} ${settings.provider === "oss" ? styles.syncTabActive : ""}`}
                    onClick={() => patch({ provider: settings.provider === "oss" ? "" : "oss" })}
                  >
                    阿里云 OSS
                  </button>
                  <button
                    type="button"
                    className={`${styles.syncTab} ${styles.syncBtnGlass} ${settings.provider === "webdav" ? styles.syncTabActive : ""}`}
                    onClick={() => patch({ provider: settings.provider === "webdav" ? "" : "webdav" })}
                  >
                    WebDAV
                  </button>
                </div>
              </div>

              {settings.provider === "oss" && (
                <>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>Endpoint</span>
                    <input
                      className={styles.syncInput}
                      value={settings.oss.endpoint}
                      placeholder="https://oss-cn-hangzhou.aliyuncs.com"
                      onChange={(e) => patchOss({ endpoint: e.target.value })}
                    />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>Bucket</span>
                    <input className={styles.syncInput} value={settings.oss.bucket} placeholder="my-bucket" onChange={(e) => patchOss({ bucket: e.target.value })} />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>AccessKey ID</span>
                    <input className={styles.syncInput} value={settings.oss.accessKeyId} onChange={(e) => patchOss({ accessKeyId: e.target.value })} />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>AccessKey Secret</span>
                    <input
                      className={styles.syncInput}
                      type="password"
                      value={settings.oss.accessKeySecret}
                      onChange={(e) => patchOss({ accessKeySecret: e.target.value })}
                    />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>对象路径</span>
                    <input className={styles.syncInput} value={settings.oss.objectKey} placeholder="dtv/config-sync.json" onChange={(e) => patchOss({ objectKey: e.target.value })} />
                  </div>
                </>
              )}

              {settings.provider === "webdav" && (
                <>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>WebDAV 地址（含文件名）</span>
                    <input
                      className={styles.syncInput}
                      value={settings.webdav.url}
                      placeholder="https://dav.example.com/dtv/config-sync.json"
                      onChange={(e) => patchWebdav({ url: e.target.value })}
                    />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>用户名</span>
                    <input className={styles.syncInput} value={settings.webdav.username} onChange={(e) => patchWebdav({ username: e.target.value })} />
                  </div>
                  <div className={styles.syncField}>
                    <span className={styles.syncLabel}>密码</span>
                    <input className={styles.syncInput} type="password" value={settings.webdav.password} onChange={(e) => patchWebdav({ password: e.target.value })} />
                  </div>
                </>
              )}

              <div className={styles.syncField}>
                <label className={styles.syncLabel} style={{ display: "flex", alignItems: "center", gap: 8, cursor: "pointer" }}>
                  <input
                    type="checkbox"
                    checked={settings.autoSync}
                    onChange={(e) => patch({ autoSync: e.target.checked })}
                  />
                  自动同步（定时把本机配置上传到云端）
                </label>
                <select
                  className={styles.syncInput}
                  value={settings.intervalMinutes}
                  disabled={!settings.autoSync}
                  onChange={(e) => patch({ intervalMinutes: Number(e.target.value) })}
                >
                  {SYNC_INTERVAL_OPTIONS.map((opt) => (
                    <option key={opt.value} value={opt.value}>
                      {opt.label}
                    </option>
                  ))}
                </select>
              </div>

              <div className={styles.syncMetaGrid}>
                <div className={styles.syncMetaItem}>
                  <span className={styles.syncMetaKey}>上次上传</span>
                  <span className={styles.syncMetaVal}>{formatTime(state.lastPushAt)}</span>
                </div>
                <div className={styles.syncMetaItem}>
                  <span className={styles.syncMetaKey}>上次下载</span>
                  <span className={styles.syncMetaVal}>{formatTime(state.lastPullAt)}</span>
                </div>
                <div className={styles.syncMetaItem}>
                  <span className={styles.syncMetaKey}>上次自动同步</span>
                  <span className={styles.syncMetaVal}>{formatTime(state.lastAutoPushAt)}</span>
                </div>
              </div>

              {message && <div className={styles.syncHint}>{message}</div>}
              {error && (
                <div className={styles.syncHint} style={{ color: "#ff7b72" }}>
                  {error}
                </div>
              )}
              {state.lastError && !error && (
                <div className={styles.syncHint} style={{ color: "#ff7b72" }}>
                  最近一次自动同步失败：{state.lastError}
                </div>
              )}

              <div className={styles.syncActionsRow}>
                <button type="button" className={`${styles.syncBtnGlass} ${styles.secondaryBtn}`} disabled={!settings.provider || busy !== ""} onClick={() => void run("test")}>
                  <PlugZap size={15} />
                  {busy === "test" ? "测试中…" : "测试连接"}
                </button>
                <button type="button" className={`${styles.syncBtnGlass} ${styles.primaryBtn}`} disabled={!settings.provider || busy !== ""} onClick={() => void run("push")}>
                  <CloudUpload size={15} />
                  {busy === "push" ? "上传中…" : "上传到云端"}
                </button>
                <button type="button" className={`${styles.syncBtnGlass} ${styles.primaryBtn}`} disabled={!settings.provider || busy !== ""} onClick={() => void run("pull")}>
                  <CloudDownload size={15} />
                  {busy === "pull" ? "下载中…" : "下载并应用"}
                </button>
              </div>
      <div className={styles.syncHint}>
        同步内容：关注列表、分组、自定义分类、弹幕偏好与屏蔽词、各直播间音量。密钥只保存在本机。
      </div>
      </div>
    </>
  );
}
