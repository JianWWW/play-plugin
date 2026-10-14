# 术语表 (Domain Glossary)

监控室网页播放插件的领域词汇。实现细节见 `docs/`,这里是语言的单一事实来源。

## 流 (Stream)

- **直播流 (Live stream)**: 实时画面,无起点/终点/进度。seek、倍速、进度条对它无意义。
- **回放流 (Playback stream)**: 有时间范围的录像流(GB28181 平台回放 / NVR / 文件)。seek、倍速只属于回放流。
- **显示冻结 (Display freeze / pause)**: 插件把画面停住、声音静音,数据继续在后台排空。所有协议行为一致。协议:`stream.pause`。
- **信令暂停 (Stream pause)**: 平台/设备真正停止推流(GB28181 SIP / RTSP PAUSE)。由页面通过平台 API 发起,插件不参与。
- **流倍速 (Stream speed)**: 服务器按倍速推流(GB28181 SIP 控制后时间戳同步加速)。插件零改动天然兼容——渲染是 on-arrival 的。
- **渲染倍速 (Render speed)**: 客户端自己按倍速消费。仅对本地可配速的源(测试源/文件)有效。协议:`stream.setSpeed`,网络流返回 `UNSUPPORTED_SPEED`。
- **录像 (Recording)**: 解复用层转封装(codec copy,不解不编)为分片 MP4,断电安全。音频仅 AAC(G.711 不进 MP4)。协议:`stream.record.start/stop`。

## 日志 (Logging)

- **调试开关 (Debug switch)**: 页面通过 SDK 传入的布尔配置,页面级语义。开启后端到端生效:该页面的 SDK console 日志与原生插件的文件日志同时进入 debug 级。原生进程全机共享,故级别提升是**进程级、只升不降**——任一页面开启后保持到进程重启,不做连接引用计数。
- **日志脱敏 (Log redaction)**: 流 URL 中的凭据(`user:pass@`)在进入任何日志前一律遮蔽为 `***@`;日志中不存在明文凭据。

## 原生控制条 (Native control bar)

- **标题条 (Title bar)**: 悬浮窗顶部的流标题(监控墙标签),`stream.open` 的 `title` 配置,空则不显示。
- **控制条 (Control bar)**: 悬浮窗底部原生绘制的按钮条:`playPause` / `snapshot` / `record` / `speed`,按 `controls` 配置显示。
- **控制事件 (Control event)**: 原生按钮按下后上报页面(`app.control`)。SDK 默认绑定插件 API;页面提供 `onControl` 时接管(GB28181 的倍速/seek 走这里转平台 SIP)。
