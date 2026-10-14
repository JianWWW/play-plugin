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
| `hello` | `{client, protocol, debug?}` | `{plugin, version, protocol, maxStreams, capabilities{h264,h265,rtsp,flv,hls,rtmp,udp,hardwareDecode}}` —— 连接后第一个请求，5s 内必须发送。`debug:true` 时原生日志级别提升到 debug（进程级、只升不降，重启恢复，见 ADR-0001） |
| `stream.open` | `{streamId, url, rect{l,t,w,h}, muted, title?, controls?, speeds?}` | `{}`；随后异步事件见下。`title` 原生顶部标题条（空=不显示）；`controls` 控制条按钮子集（playPause/snapshot/record/speed，默认全部）；`speeds` 倍速档位（默认 [1,2,4,8,16]） |
| `stream.rect` | `{streamId, rect, hidden}` | `{}`；rect 为物理像素屏幕坐标；hidden=true 隐藏浮层 |
| `stream.mute` | `{streamId, muted}` | `{}` |
| `stream.pause` | `{streamId, paused}` | `{}`；显示冻结（画面停住/静音，数据继续排空）；广播 `stream.state`=paused/playing |
| `stream.seek` | `{streamId, timeMs}` | `{}`；仅可 seek 源（文件/test），其余 `UNSUPPORTED_SEEK`；在管线线程执行，2s 超时 |
| `stream.setSpeed` | `{streamId, speed}` | `{}`；仅本地配速源生效；网络流 `UNSUPPORTED_SPEED`（GB28181 走平台 SIP） |
| `stream.record.start` | `{streamId}` | `{record{path}}`；转封装 fMP4（codec copy），默认存 `%USERPROFILE%\Videos\play-plugin`；音频仅 AAC |
| `stream.record.stop` | `{streamId}` | `{record{path}}`；写 trailer 收尾 |
| `stream.snapshot` | `{streamId}` | `{path}`（本机 PNG 路径） |
| `stream.close` | `{streamId}` | `{}` |
| `app.status` | `{}` | `{version, activeStreams}` |
| `ping` | `{}` | `{pong}`（毫秒时间戳） |

`url` 仅接受 `rtsp:// rtsps:// rtmp:// rtmps:// http:// https:// udp://`（http-flv
/ hls 走 http；组播 IPTV / GB28181 平台下发的 RTP/PS 流走 `udp://`，多网卡机器
可用 `udp://…?localaddr=<网卡IP>` 绑定接收网卡）与 `test://pattern?w=&h=&fps=`
（内置测试源）。

## 事件

| event | params |
|---|---|
| `hello`（连接即推） | `{plugin, version, protocol}` |
| `stream.state` | `{streamId, state: connecting\|playing\|reconnecting\|stopped\|error, code?, message?}` |
| `stream.info` | `{streamId, codec, width, height, decoder: d3d11va\|sw\|test}` |
| `stream.stats` | `{streamId, fps, bitrateKbps, dropped, decoder}`（每秒） |
| `stream.select` | `{streamId}`（用户单击浮层窗口选中该流） |
| `app.control` | `{streamId, button, value?}`（原生控制条按钮按下；button=playPause/snapshot/record/speed，speed 带 value=新档位；发给拥有该流的连接） |
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

## 浮层鼠标交互

- 单击浮层 → 推送 `stream.select` 事件（仅发给拥有该流的连接）。
- 双击浮层 → 进入/退出全屏（覆盖整个虚拟屏幕，再次双击恢复原位置）。全屏期间
  页面推送的 `stream.rect` 被忽略，直到退出全屏。

## 预留（v2）

- `exclusion-zone`：网页弹层区域挖洞（当前用 `hidden` 规避）。
- 二进制帧（仅协议控制用不到；遥测扩展若需要再开）。
