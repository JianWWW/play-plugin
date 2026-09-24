# @playplugin/sdk

网页端 SDK：检测本地 PlayPlugin 插件、与其本地 WebSocket 通信、把网页占位元素
`mount` 映射为原生 Overlay 悬浮窗（自动跟随滚动/缩放/全屏/DPI）。

## 安装

```bash
pnpm add @playplugin/sdk
```

## 快速开始

```ts
import { detectPlugin, OverlayPlayer, setDownloadUrl } from "@playplugin/sdk";

setDownloadUrl("/downloads/PlayPlugin.msi");

const info = await detectPlugin();
if (!info) {
  showDownloadBanner(); // 引导用户下载安装
}

const player = new OverlayPlayer({
  url: "rtsp://user:pass@camera/Streaming/Channels/101",
  mount: document.getElementById("cam1")!,
  muted: true, // 默认静音
});
await player.open();

player.on("info", (i) => console.log(i.codec, i.width, i.height, i.decoder));
player.on("stats", (s) => console.log(s.fps, s.bitrateKbps, s.dropped));
player.on("state", (s) => console.log(s.state));
```

## API

| 成员 | 说明 |
|---|---|
| `detectPlugin(opts?)` | 探测本机插件，返回 `PluginInfo & { port }` 或 `null` |
| `isCompatible(info)` | 协议版本是否与本 SDK 匹配 |
| `new OverlayPlayer({ url, mount, muted?, streamId? })` | 创建播放器；url 支持 rtsp/rtsps/http-flv/hls |
| `player.open()` | 建立连接并开流；随后自动同步 mount 位置 |
| `player.close()` | 关流并回收浮层 |
| `player.setMuted(b)` | 静音开关 |
| `player.snapshot()` | 抓图，返回本机 PNG 路径 |
| `player.conceal() / reveal()` | 网页弹层需要盖住视频时临时隐藏浮层 |
| `player.on("state" \| "info" \| "stats" \| "updateAvailable", cb)` | 事件订阅 |
| `setDownloadUrl(url)` | 设置未安装时的下载引导地址 |

## 位置同步说明

`mount` 元素的视口矩形通过 `getBoundingClientRect()` + `window.screenX/screenY`
+ 浏览器 chrome 偏移 + `devicePixelRatio` 换算为**物理像素屏幕矩形**，在
ResizeObserver / scroll(rAF 节流) / resize / 全屏切换 / zoom(0.5s 轮询兜底) 时
推送给插件。页面侧无需任何额外处理。

## 事件重连

插件连接断开（含插件升级重启）时 SDK 以指数退避自动重连，重连成功后自动重开
本页所有 `OverlayPlayer` 的流并重发位置。
