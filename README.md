# PlayPlugin

Windows 原生浏览器播放插件：网页 Overlay 悬浮窗播放 RTSP / RTMP / HTTP-FLV /
HLS / UDP 组播（含 GB28181 RTP/PS）直播流，支持 H.264 / H.265，目标 **16 路并发、
RTSP 端到端 ≤1s**。回放：显示冻结（pause）、seek、倍速、本地 fMP4 录像；悬浮窗
原生绘制标题条与可配置控制条（播放暂停/截图/录像/倍速）。

浏览器检测插件 → 引导下载安装 → 页面与插件本地 WebSocket 通信 → 插件创建原生
悬浮窗贴住网页占位元素渲染（零拷贝硬解 → D3D11 上屏）。

```
┌──────────┐  WebSocket(JSON 信令)  ┌─────────────────────────────┐
│  网页     │◄─────────────────────►│  play-plugin.exe            │
│  (SDK)    │                       │  ┌─────────┐  ┌──────────┐  │
│  占位 div │                       │  │ FFmpeg   │→ │ D3D11    │  │
└──────────┘                       │  │ 解码管线  │  │ 悬浮窗渲染│  │
                                   │  └─────────┘  └──────────┘  │
                                   └─────────────────────────────┘
                                        ↑ 浏览器窗口 Owned Window
```

## 组件

| 目录 | 内容 |
|---|---|
| `crates/core` | 每路管线线程：FFmpeg 拉流/解封装/D3D11VA 硬解（回退软解）、测试源、GPU 句柄 |
| `crates/overlay` | Win32 悬浮窗（不抢焦点/不进 Alt-Tab/跟随浏览器 z 序）、D3D11 渲染、WASAPI 音频 |
| `crates/server` | axum 本地服务：/info 探测、Origin 白名单、PNA 预检、信令协议、会话/资源管理 |
| `crates/app` | 托盘、单实例、崩溃 minidump、自动更新（Authenticode 验签）、`--smoke` 自检 |
| `sdk/` | `playplugin-sdk`：探测/下载引导、流生命周期、rect 同步（滚动/缩放/F11/DPI/多显示器，浏览器骑跨混合缩放屏除外）、自动重连 |
| `installer/` | WiX MSI（静默部署、ORIGINS 预置）、最小化 FFmpeg 构建脚本、LGPL 声明 |
| `docs/` | [信令协议](docs/protocol.md)、[部署手册](docs/deployment.md)、[调试指南](docs/debugging.md) |

## 体积

| 产物 | 体积 |
|---|---|
| play-plugin.exe | ~3 MB（fat LTO / panic=abort / 符号剥离） |
| FFmpeg 5 个 DLL（最小化构建） | ~13 MB |
| **MSI 安装包** | **~5 MB** |

FFmpeg 为解码专用最小化构建（`installer/configure-ffmpeg-min.sh`）：仅
h264/hevc/aac/mp3/g711 解码器、rtsp/rtp/flv/hls/mpegts/mpegps/rtmp 解封装、d3d11va/dxva2
硬解、schannel TLS；无编码器、无滤镜、无设备层。替换官方全量 DLL（~230 MB）后
产物缩小 ~95%。

## 本地开发

前置：Rust stable (MSVC)、VS Build Tools、LLVM（libclang）、Node ≥20。

构建最小化 FFmpeg（首次，一次性）：

```powershell
# 前置：MSYS2 装到 C:\msys64，pacman -S make nasm diffutils，
# 并把 C:\msys64\usr\bin\link.exe 改名为 gnulink.exe（避免遮蔽 MSVC link.exe）
# 下载 ffmpeg-9.0.2.tar.xz 到 .deps\
installer\build-ffmpeg-min.cmd   # 产出 .deps\ffmpeg-min\（头文件+.lib+DLL）
```

无 MSYS2 时可用官方全量共享构建兜底（仅取头文件和 .lib 编译，产物体积会变大）：

```powershell
powershell -File installer/fetch-ffmpeg.ps1
# 然后把 .cargo/config.toml 的 FFMPEG_DIR 指向解压目录
```

```powershell
# 构建与测试（FFMPEG_DIR 已由 .cargo/config.toml 指向 .deps\ffmpeg-min）
cargo test --workspace
cp .deps/ffmpeg-min/bin/*.dll target/debug/   # 运行时找 DLL

# 自检（4 个测试图案浮层 5 秒）
cargo run -p plugin-app -- --smoke

# SDK
cd sdk && pnpm install && npx vitest run && npx tsc -p tsconfig.json
```

真实流验证（本地起一个 H.264 FLV）：

```bash
ffmpeg -f lavfi -i testsrc=size=640x360:rate=30 -t 300 \
  -c:v libx264 -pix_fmt yuv420p -f flv .deps/test.flv
node -e "require('http').createServer((q,s)=>{require('fs').createReadStream('.deps/test.flv').pipe(s)}).listen(8090)"
PLAY_PLUGIN_TEST_URL=http://127.0.0.1:8090/test.flv \
  cargo test -p plugin-server --test real_stream -- --ignored
```

网页手动联调页：`examples/page/index.html`。

## 安全模型

本地 WebSocket 仅监听 127.0.0.1；`Origin` 白名单（安装预置 + 配置文件）之外一律
403，防任意网页探测/操控；RTSP 凭证日志脱敏；更新包必须过 WinVerifyTrust。

## 许可证

本仓库 LGPL-2.1-or-later；FFmpeg 动态链接、DLL 可替换（见
`installer/THIRD-PARTY-NOTICES.txt`）。
