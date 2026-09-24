# PlayPlugin 信令协议 v1

传输：`ws://127.0.0.1:<port>/ws`，JSON 文本帧。视频/音频数据**不**走 WebSocket——
原生窗口直接渲染，网页只收控制面数据。

## 连接建立

1. 页面 `GET http://127.0.0.1:<port>/info` 探测（返回版本/协议号）。端口默认
   17653，被占时按配置顺延（port_fallback）。
2. 页面发起 WS 升级，插件校验 `Origin` 是否在白名单（TOML 配置；空名单 = 开发
   模式放行并告警）。拒绝返回 HTTP 403。
3. 插件实现 Chrome Private Network Access / Local Network Access 预检
   （OPTIONS 返回 `Access-Control-Allow-Private-Network: true` 等 CORS 头）。

## 帧格式

请求（页面 → 插件）：

```json
{ "v": 1, "id": 42, "method": "stream.open", "params": { ... } }
```

响应（对应 id）：

```json
{ "v": 1, "id": 42, "ok": true,  "result": { ... } }
{ "v": 1, "id": 42, "ok": false, "error": { "code": "LIMIT_STREAMS", "message": "max 32 streams" } }
```

事件（插件 → 页面，无 id）：

```json
{ "v": 1, "event": "stream.state", "params": { ... } }
```

## 方法

| method | params | result |
|---|---|---|
| `hello` | `{client, protocol}` | `{plugin, version, protocol, maxStreams, capabilities{h264,h265,rtsp,flv,hls,hardwareDecode}}` —— 连接后第一个请求，5s 内必须发送 |
| `stream.open` | `{streamId, url, rect{l,t,w,h}, muted}` | `{}`；随后异步事件见下 |
| `stream.rect` | `{streamId, rect, hidden}` | `{}`；rect 为物理像素屏幕坐标；hidden=true 隐藏浮层 |
| `stream.mute` | `{streamId, muted}` | `{}` |
| `stream.snapshot` | `{streamId}` | `{path}`（本机 PNG 路径） |
| `stream.close` | `{streamId}` | `{}` |
| `app.status` | `{}` | `{version, activeStreams}` |
| `ping` | `{}` | `{pong}`（毫秒时间戳） |

`url` 仅接受 `rtsp:// rtsps:// http:// https://`（http-flv / hls 走 http）与
`test://pattern?w=&h=&fps=`（内置测试源）。

## 事件

| event | params |
|---|---|
| `hello`（连接即推） | `{plugin, version, protocol}` |
| `stream.state` | `{streamId, state: connecting\|playing\|reconnecting\|stopped\|error, code?, message?}` |
| `stream.info` | `{streamId, codec, width, height, decoder: d3d11va\|sw\|test}` |
| `stream.stats` | `{streamId, fps, bitrateKbps, dropped, decoder}`（每秒） |
| `app.updateAvailable` | `{version, url}` |

## 心跳与回收

- 插件每 5s 发 WS Ping（浏览器自动 Pong）；30s 无任何入站帧判定为半开连接，
  回收该页面全部流与浮层窗口。
- 页面关闭/刷新 → TCP 断开 → 立即回收。
- 每个 `streamId` 归属唯一连接；多标签页各自管理各自的流。

## 错误码

`NOT_AUTHENTICATED` `UNKNOWN_METHOD` `BAD_PARAMS` `BAD_URL` `ALREADY_EXISTS`
`NOT_FOUND` `LIMIT_STREAMS` `SNAPSHOT_FAILED` `DISCONNECTED` `TIMEOUT`（后两个为
SDK 侧）。

## 预留（v2）

- `exclusion-zone`：网页弹层区域挖洞（当前用 `hidden` 规避）。
- 二进制帧（仅协议控制用不到；遥测扩展若需要再开）。
