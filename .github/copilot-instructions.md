# DTV 仓库长期约定

本文件记录**每次会话都要用到、踩过就会翻车**的事实。全局个人规矩（改完自动构建提交 push、
业务设计先问、只做范围内的活等）已在用户级 instructions 里，这里只写 DTV 特有的部分。

## 项目形态

Tauri 2 桌面 + Android 直播聚合播放器，支持 斗鱼 / 虎牙 / 抖音 / B站。

- 前端 `web/`：React + TS + Vite + xgplayer
- 后端 `src-tauri/src/`：Rust
- npm scripts 都转发到 `web/`（`dev` / `build` / `preview` / `tauri`）

关键位置：

| 路径 | 职责 |
| --- | --- |
| `web/src/components/player/MainPlayer.tsx` | 取流 / 挂载 / 切房编排（全局只有一个实例） |
| `web/src/components/player/playerControlsPlugins.ts` | xgplayer 控制插件，含 `VolumeControl` |
| `web/src/components/player/constants.ts` | 音量与弹幕偏好的存储契约 |
| `web/src/services/lanSync.ts` | 云 / 局域网配置合并引擎 |
| `web/src/services/cloudSync.ts` | 同步调度与 OSS 计费闸门 |
| `web/src/platforms/` | 各平台取流与画质 |
| `src-tauri/src/proxy.rs`、`flv_relay.rs` | 本地代理与 FLV 中继 |
| `src-tauri/src/cloud_sync.rs` | OSS / WebDAV 读写 |

## 验证命令（别用错）

- **类型检查必须** `npx tsc -p web/tsconfig.json --noEmit`。
  根目录 `npx tsc --noEmit` 是**假绿**：根 `tsconfig.json` 的 include 是 `src/**`，
  而仓库根**没有 `src/` 目录**，等于一个文件都没查。
- 前端构建：`npm -C web run build`
- Rust：`cargo test --lib <filter>`，并发 flaky 时加 `-- --test-threads=1`
- 仓库**没有 ESLint 配置 / lint script**，web 侧没有可用的格式化检查，不要凭空造一个。

## 构建工具链（最容易翻车）

### 桌面 dmg

**构建前必须** `export PATH="$HOME/.cargo/bin:$PATH"`。
shell 默认 `rustc` 是 Homebrew stable，而 `src-tauri/rust-toolchain.toml` 钉的是
`channel = "nightly"`；用错会在 `tars-stream` 报 **E0554**（`#![feature]` 不能在 stable 上用）。

```bash
export PATH="$HOME/.cargo/bin:$PATH"
./node_modules/.bin/tauri build --bundles dmg
```

用仓库本地的 `node_modules/.bin/tauri`，别依赖 `pnpm`：本机 pnpm 走 corepack，缓存坏过
（2026-11-19 报 `Cannot find module .../corepack/v1/pnpm/.../pnpm.cjs`）。
tauri.conf.json 的 beforeBuildCommand 走 npm，不受 pnpm 影响。

### Android apk

环境变量每条 export **单独一行**（同一句里后一个赋值引用不到前一个）：

```bash
export PATH="$HOME/.cargo/bin:$PATH"
export ANDROID_HOME=$HOME/Library/Android/sdk
export NDK_HOME=$HOME/Library/Android/sdk/ndk/27.0.11718014
export BINDGEN_EXTRA_CLANG_ARGS="--sysroot=$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/sysroot"
./node_modules/.bin/tauri android build --apk
```

- 本机只装了 NDK `27.0.11718014`，别写别的版本号；缺 `BINDGEN_EXTRA_CLANG_ARGS` 会报 `stdio.h not found`。
- **dmg 与 apk 必须串行**：同一个 cargo workspace 锁，并行会互相阻塞/失败。
- 产物 `src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk`
  —— **文件名不带版本号**，拷之前先看 mtime 确认是本轮新包。
- 签名（本机用 debug key）：

```bash
BT=$ANDROID_HOME/build-tools/36.0.0
$BT/zipalign -f -p 4 <unsigned.apk> /tmp/aligned.apk
$BT/apksigner sign --ks ~/.android/debug.keystore \
  --ks-pass pass:android --key-pass pass:android \
  --out ~/Downloads/DTV-<版本号>-universal.apk /tmp/aligned.apk
```

## 版本号

用户可感知的改动要**同步 bump 三处**：`package.json`、`web/package.json`、`src-tauri/tauri.conf.json`。

`src-tauri/Cargo.toml` 的版本号**有意不动**（停在 3.1.73），不要去「顺手对齐」。

提交信息格式：`<版本号>: <中文描述>`。

## 任务完成后：自动提交 push + 出包（本仓库例外，2026-11-19 用户确认）

全局规矩是「每次 commit 必须逐次确认」（2026-11-12 铁律）；**本仓库例外**：用户已常设授权，
任务完成后**自动 commit + 自动 push，不再逐次询问**，并构建最新 dmg 与 apk。

- 前提不变：改动本身要先经用户同意（未经同意不动代码）；只提交本轮范围内的改动。
- 顺序沿用全局流程：验证 → 构建 dmg → 构建 apk（串行）→ 拷产物到 `~/Downloads` → commit → push。
  纯文档 / instructions 改动也出包；这类改动不算用户可感知，不 bump 版本号。
- commit message 沿用 `<版本号>: <中文描述>` + Copilot trailer；push 到 origin 当前分支。
- 构建或验证失败：不 commit 不 push，先报告失败原因。push 遇远程更新 / hooks 拦截：停下报告，不 force、不 amend。
- 用户当轮明确说「不要构建 / 不要提交 / 不要 push」时以当轮指示为准；只读任务不触发本流程。

## 播放器音量语义（改这块前先读）

房间 key = `` `${platform}/${roomId}` ``。存储契约在 `constants.ts`：

- `dtv_player_volume_v1`：全局默认音量，标量字符串
- `dtv_player_volume_room_v1`：map `"<PLATFORM>/<roomId>": 0..1`，上限 300 条，淘汰最早插入

**「绑定房间」与「实际出声」是分开的两步，不能合并：**

- `setRoomKey()` 只改记账 key，**绝不碰 `player.volume`**
- `applyRoomVolume()` 才真正应用 `loadRoomVolume(key) ?? loadStoredVolume()`，
  且只在**新流真正接上后**调用（挂载路径 + 四个平台的 `softSwitchAndApplyVolume`）

原因：React 的 `[platform, roomId]` effect 远早于异步取流完成。早期版本在 effect 里
直接应用音量，会把新房间的音量写到**还在播的旧流**上，用户听到「突然放大一下再切」。

其它不变量：

- 未调过音量的房间用**全局默认**，不沿用上一个房间的音量。
- 切房窗口内旧流保持自己的音量，**不做静音**。
- `applying` + `appliedValue` 守卫必须保留：它阻止程序化改音量被 `volumechange` 监听器写回存储。
- 切房窗口内拖滑块存到**新**房间；`persistVolume` 仅在 volume > 0 时写全局，房间记忆可以存 0。
- 软切（`player.switchURL`）条件：同播放类型（hls/flv）且 `danmuOverlayRef.current` 存在；
  否则走 `destroyPlayer()` + `mountPlayer()`。软切时播放器与所有插件存活。

## 虎牙断流自愈（3.1.84 起，改这块前先读）

三层机制，全在既有看门狗体系内，别另起炉灶：

- **代理自动换线**（`proxy.rs` + `huya/stream_url.rs`）：重签仍被拒（fresh 判定 =
  `huya_sign_age_ms() < 240s`）或短命会话（`session_bytes>0 && lived<60s`）→
  `fetch_huya_flv_url_rotating` 按候选序 tx→al→hs 循环换线。生效线路存
  `StreamUrlStore.huya_fallback_line`，**不写用户偏好**（localStorage 线路偏好不动）；
  `set_stream_url_cmd`（切房/切画质/手动换线）清空它回到偏好线路。换线成功发
  Tauri 事件 `huya-line-fallback` `{room, line}`，前端只同步徽标。
- **静默断流即重载+换线（3.1.86 起）**：虎牙上游 8s 零字节（`UPSTREAM_SILENCE_TIMEOUT`）
  时，pump 在 silent 分支里**先** `refresh_store_url(rotate=true)` 换线重签、发
  `huya-upstream-lost` `{room, line}`（line=生效线路，换线失败也发、留在原线），
  **再** `commit_new_source` bump gen——顺序不能反，refresh 的 stale 守卫按当前
  gen 校验，先 bump 会把换线结果当切房丢弃。前端收到事件立即
  `reloadStream("refresh", { line })`（8s 冷却 + 共享 `reconnectAttemptRef` 预算，
  `playerIsLive === false` 不触发）。背景：gen bump 只在 pump 重连后**广播出
  新 gen 字节**时才切断客户端响应；CDN 死透时重连也拿不到字节，播放器永远等不到
  ended，只能靠 15s waiting 计时器（2026-11-23 实测用户手点刷新才 5s 恢复）。
- **签名年龄有两套戳，别混用**：`LAST_SIGN_MS`（全局）被 `commit_new_source` 重置，
  量不出 URL 真实年龄；`HUYA_LAST_SIGN_MS` 只在虎牙真签名时写（refresh 成功 +
  `set_stream_url_cmd` platform=huya），换线触发判定只认它。
- **前端浅缓冲垫底**（MainPlayer 看门狗 effect 内）：`timeupdate` 捕获监听（document
  capture，免逐元素 attach、重载后立即生效），虎牙缓冲 <4s 时 `playbackRate=0.95`
  攒垫，≥4s 恢复 1.0；连续慢放 120s 封顶，缓冲达标一次后才重新武装。
  xgplayer 的 playbackRate 插件是关的（`playbackRate: false`），别打开。
- **恢复提速**：`ended` 捕获监听即刻重载（与重播层轮询共享 8s 冷却 + 6 次预算，
  `playerIsLive === false` 不触发）；虎牙 `readyState<2`（无数据）冻结判定 2 样本
  （10s），其余仍 3 样本（15s）。
- **事件同步线路不能触发重载**：`proxyLineSyncRef` 存同步时的画质快照，
  `currentLine` 变化的 debounce effect 里只有画质没变才吞掉重载。

## 播放器自愈监听（3.1.85 起，改这块前先读）

- `error` / `waiting` / `ended` 自愈监听注册在 `mountPlayer` 里，但**软切换
  （`switchURL`）不重建播放器**，监听会比挂载会话活得久；`reloadStream` 每次
  ++session。3.1.85 前这些监听用挂载期 `isSessionActive(sessionId)` 守卫 →
  第一次软切换 reload 后全部失效：解码错误（5103 / MediaError 3）只能等 15s
  冻结看门狗，黑屏十几秒（抖音 258275062804 真机复现：首次 2s 自愈、二次 18s）。
- 现在守卫是 `disposedRef.current || playerRef.current !== player`（跟随存活的
  播放器实例），日志标签用 `streamLabelRef.current`（先例：`playing` 监听）。
  **别改回 sessionId 守卫**；新加播放器事件自愈监听照此写。
- 去抖/预算（decode 8s、ended 3s、waiting nudge 8s、MAX_AUTO_RECONNECT）是
  mount 闭包局部量，重挂载自然重置；`autoReconnectTimerRef` 是组件级共享，保持现状。
- xgplayer replay 覆盖层只在 `ENDED` 显示；解码错误不触发 ended，看门狗的重播层
  轮询救不了它，只能靠 decode 自愈或冻结判定——所以 decode 监听失效的代价是整段
  15s+ 黑屏。

## 云同步语义（改这块前先读）

- **条目级增量合并**：local / remote / prevItems 按 id 求并集，胜者按 ledger 时间戳。
- **remote 里没有 ≠ 删除**；只有显式 `deletions` 才打 tombstone。
- `followListOrder` 由 `rebuildFollowOrder` 重建，不要手工拼。
- 身份规则：分组**按名字**折叠（`foldRemoteFoldersByName`，同名不同 id 折进本地 id、成员求并）；
  主播**同平台 + 同昵称（小写）不同 id 视为重复并跳过**（`dropRemoteStreamerTwins`，
  必须在折叠**之前**跑，带 `excludedMemberKeys`）。
- 计费闸门：指纹门控 + 时间戳锚定 + `HEARTBEAT_MIN_HOURS = 24` 心跳下限 +
  内容比对守卫。**目的是避免频繁 PUT OSS 产生费用**，加同步触发点前先想清楚会不会破坏门控。
- `oss_get` 的 404 必须读 body：只有 `<Code>NoSuchKey</Code>` 或空 body 才 `Ok(None)`，
  否则返回 `Err`。否则 NoSuchBucket 会伪装成「云端没对象」，每轮白白 PUT 一次。
- 局域网导入 `applyIncrementalLanSyncImport` 本来就按名字匹配分组；云路径已与之对齐，
  **LAN 路径有意未动**，不要顺手改。

## 真机取证

### 实时日志

`~/Downloads/DTV-live.log`（一直在写，可能几 MB，用 `grep -a` 定向查，别整份读）。

- `grep -a "room volume" DTV-live.log` → `bound` / `applied … 来源:` / `skip` / `saved`，
  用来判断音量归因是否正确
- 云同步排查看 `frontend_log` 相关行与 skip 统计

### WKWebView localStorage

改真机的关注列表 / 音量记忆时用：

```
~/Library/WebKit/com.dtv.app/WebsiteData/Default/<salt>/<salt>/LocalStorage/localstorage.sqlite3
```

（连同 `-wal`、`-shm`；`<salt>` 目录名需实地 `ls` 确认）

- value 是 **UTF-16LE blob**：读 `select hex(value)` → `bytes.fromhex(...).decode("utf-16-le")`；
  写 `json.dumps(...).encode("utf-16-le")` 作为 `sqlite3.Binary`。
- **写之前必须完全退出 App**。本机 App **忽略 SIGTERM**，需要 `kill -9 <pid>`。
- 动之前先备份整个 sqlite3（含 wal/shm）。

## 会话交接

仓库根的 `HANDOFF.md` 是**跨会话交接摘要**，已被 `.gitignore` 忽略、不进版本库。
新会话开场读它 + `git log -3` 即可续接；会话收尾时**增量更新同一份**，不要每个会话新建一份。
