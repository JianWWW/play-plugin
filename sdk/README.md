# playplugin-sdk

网页端 SDK：检测本地 PlayPlugin 插件、与其本地 WebSocket 通信、把网页占位元素
`mount` 映射为原生 Overlay 悬浮窗（自动跟随滚动/缩放/全屏/DPI）。

## 安装

```bash
pnpm add playplugin-sdk
```

## 快速开始

```ts
import { detectPlugin, OverlayPlayer, setDownloadUrl } from "playplugin-sdk";

setDownloadUrl("/downloads/PlayPlugin.msi");

const info = await detectPlugin();
if (!info) {
  showDownloadBanner(); // 引导用户下载安装
}

const player = new OverlayPlayer({
  url: "rtsp://user:pass@camera/Streaming/Channels/101",
  mount: document.getElementById("cam1")!,
  muted: true, // 默认静音（监控场景）
  title: "前门枪机-01", // 原生窗口顶栏标题，空串 = 无顶栏
});
await player.open();

player.on("info",  (i) => console.log(i.codec, i.width, i.height, i.decoder));
player.on("stats", (s) => console.log(s.fps, s.bitrateKbps, s.dropped));
player.on("state", (s) => console.log(s.state, s.message ?? ""));
```

更完整的示例见 `examples/page/index.html`（宫格选择、抓图、conceal 等）。

## 全局 API

### `detectPlugin(opts?)`

探测本机插件（逐个探测 [17653, 17654, 17655, 17656] 端口的 `http://127.0.0.1:<port>/info`），
返回 `DetectedPlugin | null`（`null` = 未安装 → 展示下载引导）。

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `opts.ports` | `number[]` | `DEFAULT_PORTS` | 探测端口列表，按序尝试 |
| `opts.timeoutMs` | `number` | `800` | 单端口 fetch 超时 |

```ts
interface DetectedPlugin {
  plugin: "PlayPlugin";   // 固定标识
  version: string;        // 插件版本
  protocol: number;       // 协议版本（当前 1）
  port: number;           // 实际应答的本地端口
}
```

### `isCompatible(info)`

检测到的插件 `protocol` 是否与本 SDK 匹配（严格相等才兼容）。

### `ensurePlugin(onMissing?, opts?)`

`detectPlugin` + 兼容性检查的便捷封装：不兼容或未安装时回调 `onMissing()` 并返回
`null`，否则返回插件信息。

### `setDownloadUrl(url)` / `downloadUrl`

设置/读取下载引导地址（导出变量 `downloadUrl` 默认 `/play-plugin/PlayPlugin.msi`）。

### `configure({ debug })`

全局配置，**须在创建首个播放器之前调用**。`debug: true` 开启 SDK console 日志
（前缀 `[playplugin][conn|protocol|player|rect]`），并通过 hello 让原生日志级别
提升到 debug（进程级、只升不降，见 ADR-0001 与 `docs/debugging.md`）。

### `PluginError`

所有协议层失败的错误类型：`err.code`（如 `NOT_INSTALLED`、`TIMEOUT`）+ `err.message`。

## OverlayPlayer

### 构造参数 `PlayerOptions`

| 参数 | 类型 | 必填 | 默认 | 说明 |
|---|---|---|---|---|
| `url` | `string` | ✅ | — | 流地址。支持 `rtsp://` `rtsps://` `rtmp://` `rtmps://` `http(s)://…flv/m3u8` `udp://`（组播/GB28181 PS）、`test://`；不支持直接抛错 |
| `mount` | `HTMLElement` | ✅ | — | 页面占位元素，原生窗口 1:1 跟随其视口矩形 |
| `muted` | `boolean` | — | `true` | open 时初始静音状态 |
| `streamId` | `number` | — | 自增 | 显式流 id（同页多个 SDK 实例共享连接时用） |
| `title` | `string` | — | `""` | 原生顶栏标题（监控墙标签）；空串 = 无顶栏 |
| `controls` | `string[]` | — | 四项全开 | 原生控制条按钮，取值 `"playPause" \| "snapshot" \| "record" \| "speed"` |
| `speeds` | `number[]` | — | `[1,2,4,8,16]` | speed 按钮循环切换的倍速档位 |
| `onControl` | `(ev: ControlEvent) => void` | — | SDK 默认行为 | 接管控制条按键（GB28181 回放倍速/seek 需走平台 SIP 时使用） |

```ts
interface ControlEvent {
  streamId: number;
  button: "playPause" | "snapshot" | "record" | "speed";
  value?: number; // 仅 speed：新选中的倍速档
}
```

### 实例方法

| 方法 | 返回 | 说明 |
|---|---|---|
| `open()` | `Promise<void>` | 建立共享连接并开流（`stream.open`），随后自动同步 mount 矩形。未装插件时 reject `NOT_INSTALLED` |
| `close()` | `Promise<void>` | 关流并回收浮层；可重复调用 |
| `syncRect()` | `void` | 手动推一次当前位置/可见性（一般无需调用，SDK 已自动同步） |
| `conceal()` / `reveal()` | `void` | 网页弹层需要盖住视频时临时隐藏/恢复浮层 |
| `setMuted(b)` | `Promise<void>` | 静音开关 |
| `setPaused(b)` | `Promise<void>` | 画面冻结（画面保持、声音静默，流继续消费） |
| `seek(timeMs)` | `Promise<void>` | 仅可 seek 源（文件/`test://`）；GB28181/RTSP 回放应走平台 close+reopen，其余源 reject `UNSUPPORTED_SEEK` |
| `setSpeed(n)` | `Promise<void>` | 仅本地配速源生效；网络流 reject `UNSUPPORTED_SPEED`（GB28181 走平台 SIP） |
| `recordStart()` | `Promise<string>` | 转封装 MP4 录制（codec copy），resolve 本机文件路径 |
| `recordStop()` | `Promise<string>` | 停止录制，resolve 本机文件路径 |
| `snapshot()` | `Promise<string>` | 抓 PNG，resolve 本机文件路径 |
| `on(event, cb)` / `off(event, cb)` | `void` | 事件订阅/退订 |

### 事件

| 事件 | payload | 触发时机 |
|---|---|---|
| `state` | `StreamStateEvent` | 流状态变化 |
| `info` | `StreamInfo` | 解析出流的编解码信息（一次） |
| `stats` | `StreamStats` | 每秒统计 |
| `select` | `{ streamId }` | 单击原生浮层窗口 |
| `control` | `ControlEvent` | 控制条按键（仅未设置 `onControl` 时，此为 SDK 默认行为处理前的通知） |
| `updateAvailable` | `{ version, url }` | 插件有新版本（所有 player 广播） |

```ts
type StreamState = "connecting" | "playing" | "paused"
                 | "reconnecting" | "stopped" | "error";

interface StreamStateEvent { streamId: number; state: StreamState; code?: string; message?: string }
interface StreamInfo  { streamId: number; codec: string; width: number; height: number;
                        decoder: string /* "d3d11va" | "sw" | "test" */ }
interface StreamStats { streamId: number; fps: number; bitrateKbps: number;
                        dropped: number; decoder: string }
```

## 位置与可见性同步

`mount` 的视口矩形经 `getBoundingClientRect()` + `window.screenX/screenY` +
浏览器 chrome 偏移 + `devicePixelRatio` 换算为**物理像素屏幕矩形**，在
ResizeObserver / scroll（rAF 节流）/ resize / 全屏切换 / zoom（0.5s 轮询兜底）时
推给插件。页面侧无需任何额外处理。

浮层可见性与 `mount` 元素 1:1：

- 元素不可见（被裁剪、滚出视口、祖先 `display:none`、切标签页、浏览器失焦/被其他
  应用遮挡）→ 浮层自动隐藏，恢复可见后自动重现；
- 元素被移出文档（`remove()` / 父节点销毁）→ 流自动关闭、浮层销毁，无需手动 `close()`；
- 双击浮层进入原生全屏期间，页面推送的位置被忽略，退出后恢复跟随。

## 连接与重连

整页共享一条 WebSocket（连接复用、心跳、请求-响应配对）。插件连接断开（含插件升级
重启）时 SDK 以指数退避（500ms 起、上限 5s）自动重连，重连成功后自动重开本页所有
`OverlayPlayer` 的流并重发位置。

## 错误码

SDK 侧：`NOT_INSTALLED` `CONNECT_FAILED` `NOT_CONNECTED` `DISCONNECTED` `TIMEOUT`（请求 10s 超时）。
插件侧：`UNKNOWN_METHOD` `BAD_PARAMS` `BAD_URL` `ALREADY_EXISTS` `NOT_FOUND`
`LIMIT_STREAMS` `SNAPSHOT_FAILED` `UNSUPPORTED_SEEK` `UNSUPPORTED_SPEED` 等，
完整协议见 `docs/protocol.md`。
