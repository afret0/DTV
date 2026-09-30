"use client";

import React, { useCallback, useEffect, useState } from "react";
import { AnimatePresence, m } from "framer-motion";
import { Cast, Tv, Smartphone, X, Loader2, CheckCircle, AlertCircle } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { QRCodeCanvas } from "qrcode.react";
import { useDlnaDiscovery } from "@/hooks/useDlnaDiscovery";

interface CastInfo { stream_url: string; local_ip: string; lan_url: string; }
interface DlnaDevice { name: string; location: string; host: string; control_url: string; }

export function CastDialog({ open, onClose, platform, roomId, onCastSuccess }: {
  open: boolean; onClose: () => void; platform: string; roomId: string; onCastSuccess?: () => void;
}) {
  const { devices: globalDevices, discovering, refresh: discover } = useDlnaDiscovery();
  const [castInfo, setCastInfo] = useState<CastInfo | null>(null);
  const [dlnaDevices, setDlnaDevices] = useState<DlnaDevice[]>([]);
  const [manualIP, setManualIP] = useState("");
  const [pushingDevice, setPushingDevice] = useState<string | null>(null);
  const [pushResult, setPushResult] = useState<{ success: boolean; message: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [showManual, setShowManual] = useState(false);

  const loadCastInfo = useCallback(async () => {
    try { setError(null); setCastInfo(await invoke<CastInfo>("get_cast_info")); }
    catch (e) { setError(e instanceof Error ? e.message : String(e)); }
  }, []);

  const doPush = useCallback(async (device: { name: string; location: string; host: string }, useManualLocation?: boolean) => {
    if (!castInfo) return;
    setPushingDevice(device.host);
    setPushResult(null);
    try {
      let location = device.location;
      if (useManualLocation) {
        const ip = device.host.trim();
        location = `http://${ip}:49494/description.xml`;
      }
      await invoke("push_to_dlna", { deviceLocation: location, streamUrl: castInfo.lan_url });
      setPushResult({ success: true, message: `已推送到 ${device.name}` });
      onCastSuccess?.();
    } catch (e) {
      setPushResult({ success: false, message: e instanceof Error ? e.message : String(e) });
    } finally { setPushingDevice(null); }
  }, [castInfo, onCastSuccess]);

  const copyUrl = useCallback(() => {
    if (!castInfo) return;
    navigator.clipboard.writeText(castInfo.lan_url).then(() => { setCopied(true); setTimeout(() => setCopied(false), 2000); }).catch(() => {});
  }, [castInfo]);

  const pushManual = useCallback(async () => {
    if (!manualIP.trim() || !castInfo) return;
    await doPush({ name: `电视 (${manualIP.trim()})`, location: `http://${manualIP.trim()}:49494/description.xml`, host: manualIP.trim() }, true);
  }, [manualIP, castInfo, doPush]);

  useEffect(() => { if (open) { setDlnaDevices(globalDevices ?? []); } }, [open, globalDevices]);
  useEffect(() => {
    if (!open) return;
    loadCastInfo();
    setPushResult(null); setError(null);
    discover();
  }, [open]);

  if (!open) return null;

  return (<AnimatePresence><m.div style={s.backdrop} initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={onClose}>
    <m.div style={s.card} initial={{ opacity: 0, y: 10, scale: 0.985 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.99 }} onMouseDown={(e: React.MouseEvent) => e.stopPropagation()}>
      <div style={s.header}><div style={s.titleRow}><Cast size={18} /><span style={s.title}>投屏到电视</span></div><button style={s.closeBtn} onClick={onClose}><X size={16} /></button></div>
      <div style={s.body}>
        {error ? <div style={s.errorBox}><AlertCircle size={16} color="#ef4444" /><span>{error}</span></div> : null}
        {castInfo ? (<>
          <div style={s.qrSection}><div style={s.qrWrap}><QRCodeCanvas value={castInfo.lan_url} size={180} includeMargin level="M" bgColor="#ffffff" fgColor="#111827" /></div><p style={s.qrHint}>电视/手机浏览器扫码播放</p></div>
          <div style={s.urlSection}><div style={s.urlLabel}>投屏地址</div><div style={s.urlRow}><code style={s.urlText}>{castInfo.lan_url}</code><button style={s.copyBtn} onClick={copyUrl}>{copied ? <CheckCircle size={14} color="#22c55e" /> : "复制"}</button></div></div>
          <div style={s.divider} />
          <div style={s.dlnaSection}>
            <div style={s.dlnaHeader}>
              <span style={s.dlnaTitle}>DLNA 设备</span>
              <div style={{ display: "flex", gap: 6 }}>
                <button onClick={() => { setShowManual(true); setManualIP(""); }} style={s.smallBtn} type="button">手动输入IP</button>
                <button onClick={(e) => { e.preventDefault(); e.stopPropagation(); discover(); }} style={{ ...s.discoverBtn, opacity: discovering ? 0.6 : 1 }} disabled={discovering} type="button">{discovering ? <Loader2 size={12} className="spin" /> : null}{discovering ? "搜索中" : "搜索"}</button>
              </div>
            </div>
            {showManual ? (<div style={{ marginBottom: 10, display: "flex", gap: 6 }}>
              <input style={{ flex: 1, padding: "6px 10px", borderRadius: 8, border: "1px solid var(--border)", background: "var(--bg-primary)", color: "var(--primary-text)", fontSize: 12, fontWeight: 600 }} placeholder="电视IP地址, 如 192.168.1.100" value={manualIP} onChange={e => setManualIP(e.target.value)} onKeyDown={e => e.key === "Enter" && pushManual()} />
              <button onClick={pushManual} disabled={!manualIP.trim() || !!pushingDevice} style={{ ...s.smallBtn, background: "var(--accent)", color: "#fff" }} type="button">{pushingDevice ? <Loader2 size={12} className="spin" /> : "推送"}</button>
            </div>) : null}
            {pushResult ? (<div style={{ ...s.resultBox, background: pushResult.success ? "rgba(34,197,94,0.1)" : "rgba(239,68,68,0.1)", borderColor: pushResult.success ? "rgba(34,197,94,0.3)" : "rgba(239,68,68,0.3)" }}>{pushResult.success ? <CheckCircle size={14} color="#22c55e" /> : <AlertCircle size={14} color="#ef4444" />}<span style={{ color: pushResult.success ? "#22c55e" : "#ef4444" }}>{pushResult.message}</span></div>) : null}
            {dlnaDevices.length === 0 && !discovering ? (<p style={s.emptyHint}><Tv size={14} />{showManual ? "上方输入电视IP手动推送" : "点击搜索或在下方输入电视IP"}</p>) : null}
            {dlnaDevices.map(d => (<button key={d.location} style={s.deviceBtn} onClick={(e) => { e.preventDefault(); e.stopPropagation(); doPush(d); }} disabled={pushingDevice === d.host} type="button"><Tv size={16} /><span style={s.deviceName}>{d.name}</span>{pushingDevice === d.host ? <Loader2 size={14} className="spin" /> : <Smartphone size={14} opacity={0.5} />}</button>))}
          </div>
        </>) : (<div style={s.loadingBox}><Loader2 size={20} className="spin" /><span>获取投屏信息...</span></div>)}
      </div>
    </m.div>
  </m.div></AnimatePresence>);
}

const s: Record<string, React.CSSProperties> = {
  backdrop: { position: "fixed", inset: 0, background: "rgba(0,0,0,0.45)", display: "flex", alignItems: "center", justifyContent: "center", padding: 18, zIndex: 300 },
  card: { width: "min(420px, calc(100vw - 36px))", borderRadius: 18, background: "var(--bg-secondary)", border: "1px solid var(--border)", boxShadow: "var(--shadow-lg)", overflow: "hidden" },
  header: { display: "flex", alignItems: "center", justifyContent: "space-between", padding: "12px 14px", borderBottom: "1px solid var(--border)" },
  titleRow: { display: "flex", alignItems: "center", gap: 8, color: "var(--primary-text)", fontWeight: 700, fontSize: 14 },
  title: { fontWeight: 700 }, closeBtn: { width: 34, height: 34, borderRadius: 12, display: "inline-flex", alignItems: "center", justifyContent: "center", color: "var(--secondary-text)", border: "none", background: "none", cursor: "pointer" },
  body: { padding: 14 }, qrSection: { textAlign: "center", marginBottom: 14 }, qrWrap: { display: "inline-block", padding: 10, borderRadius: 14, border: "1px solid var(--border)", background: "rgba(0,0,0,0.22)" },
  qrHint: { fontSize: 12, fontWeight: 600, color: "var(--secondary-text)", marginTop: 8 },
  urlSection: { marginBottom: 12 }, urlLabel: { fontSize: 12, fontWeight: 700, color: "var(--secondary-text)", marginBottom: 6 },
  urlRow: { display: "flex", gap: 8, alignItems: "center" },
  urlText: { flex: 1, fontSize: 11, fontWeight: 600, color: "var(--primary-text)", padding: "6px 10px", borderRadius: 8, border: "1px solid var(--border)", background: "color-mix(in srgb, var(--hover-bg) 65%, transparent)", wordBreak: "break-all" as const },
  copyBtn: { padding: "6px 12px", borderRadius: 8, border: "1px solid var(--border)", background: "var(--hover-bg)", color: "var(--primary-text)", fontSize: 12, fontWeight: 600, cursor: "pointer", whiteSpace: "nowrap" as const },
  divider: { height: 1, background: "var(--border)", margin: "14px 0" },
  dlnaSection: {}, dlnaHeader: { display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 },
  dlnaTitle: { fontSize: 13, fontWeight: 700, color: "var(--primary-text)" },
  smallBtn: { padding: "5px 10px", borderRadius: 8, border: "1px solid var(--border)", background: "var(--hover-bg)", color: "var(--primary-text)", fontSize: 11, fontWeight: 600, cursor: "pointer" },
  discoverBtn: { padding: "5px 10px", borderRadius: 8, border: "1px solid var(--border)", background: "color-mix(in srgb, var(--accent) 20%, transparent)", color: "var(--accent)", fontSize: 11, fontWeight: 700, cursor: "pointer", display: "flex", alignItems: "center", gap: 4 },
  deviceBtn: { display: "flex", alignItems: "center", gap: 10, width: "100%", padding: "10px 12px", borderRadius: 12, border: "1px solid var(--border)", background: "color-mix(in srgb, var(--hover-bg) 65%, transparent)", color: "var(--primary-text)", fontSize: 13, fontWeight: 600, cursor: "pointer", marginBottom: 6, textAlign: "left" as const },
  deviceName: { flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" as const },
  emptyHint: { display: "flex", alignItems: "center", gap: 8, fontSize: 12, fontWeight: 600, color: "var(--secondary-text)", padding: "12px 0" },
  errorBox: { display: "flex", alignItems: "center", gap: 8, padding: "10px 12px", borderRadius: 10, background: "rgba(239,68,68,0.1)", border: "1px solid rgba(239,68,68,0.3)", color: "#ef4444", fontSize: 12, fontWeight: 600, marginBottom: 12 },
  resultBox: { display: "flex", alignItems: "center", gap: 8, padding: "10px 12px", borderRadius: 10, border: "1px solid", fontSize: 12, fontWeight: 600, marginBottom: 10 },
  loadingBox: { display: "flex", alignItems: "center", justifyContent: "center", gap: 8, padding: 40, color: "var(--secondary-text)", fontSize: 13, fontWeight: 600 },
};
