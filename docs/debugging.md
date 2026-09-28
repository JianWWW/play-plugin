# 调试指南

## 1. 最快路径：带控制台日志直接跑

debug 构建自动镜像日志到控制台，无需任何配置：

```bash
cargo run -p plugin-app -- --smoke          # 4 个测试浮层 + 完整日志
RUST_LOG=debug cargo run -p plugin-app      # 全量 debug 日志
RUST_LOG="play_server=trace,play_core=debug" cargo run -p plugin-app
```

日志优先级：`RUST_LOG` 环境变量 > `config.toml` 的 `log_level` > `info`。
release 构建默认不占控制台（开机自启不闪黑框），需要时设 `PLAY_PLUGIN_CONSOLE=1`。

## 2. 断点调试（VS Code）

已配置 `.vscode/launch.json`，装一个调试器扩展即可 F5：

| 扩展 | 断点质量 | 说明 |
|---|---|---|
| **C/C++**（ms-vscode.cpptools，`cppvsdb`） | 变量/结构体展开较粗糙 | 与 MSVC 工具链原生匹配，推荐默认 |
| **CodeLLDB**（vadimcn，`lldb`） | Rust 类型/枚举展示最好 | 需在设置里选 lldb |

三个入口：
- 「调试: 启动插件」— 直接断点 `crates/` 下任意代码
- 「调试: --smoke 自检」— 快速复现渲染/窗口路径
- 「调试: 附加到运行中的插件」— 已常驻的进程（找到 play-plugin.exe 附加）

RustRover / Visual Studio 2022 同理：VS 用「调试 → 附加到进程」，PDB 在
`target/debug/play-plugin.pdb`，源码映射自动生效。

## 3. FFmpeg / 解码问题

- FFmpeg 是外部 DLL（gyan.dev 构建无符号），**调不进去**，靠日志：
  `RUST_LOG=play_core=debug`，关注 `d3d11va unavailable` / `stream error` 行。
- 硬解是否生效看 `stream.info` 事件的 `decoder` 字段（`d3d11va` / `sw`；运行期硬解连续报错或只进包不出帧时自动切软解，值变为 `sw(auto-fallback)`，不重新拉流）。
- 快速复现某路流：`PLAY_PLUGIN_TEST_URL=<url> cargo test -p plugin-server
  --test real_stream -- --ignored --nocapture`（见 README）。

## 4. UI 线程 / 悬浮窗问题

```bash
PLAY_PLUGIN_UI_TRACE=1 cargo run -p plugin-app -- --smoke
```
每轮循环打点（cmds / frames+present / pre-pump 耗时），可定位消息泵卡死。
窗口是否创建看日志 `overlay window created stream_id=N owner=M`（owner=0
表示没找到浏览器宿主窗口，退化为置顶层窗口）。

## 5. 崩溃 / minidump

崩溃转储在 `%APPDATA%\PlayPlugin\crash\*.dmp`：

- VS：文件 → 打开 → 项目/解决方案 → 选 .dmp →「使用仅限本机进行调试」
- WinDbg：`.exr -1` 看异常，`.ecxr` 切到崩溃上下文，`k` 看栈
- Rust panic 也写日志（`play_plugin::crash`）后才生成 dump

## 6. 协议 / 信令问题

- 常驻日志里搜 `client connected` / `stream open` / `origin rejected`。
- 不装插件、纯协议联调：`crates/server/tests/integration.rs` 就是现成的
  WS 客户端样例，可复制改造成手搓报文重放。
- 手动抓包：WebSocket 走 127.0.0.1 明文 JSON，Wireshark 过滤
  `tcp.port == 17653` 即可看全部报文。

## 7. 常见问题速查

| 现象 | 先看哪里 |
|---|---|
| 页面连不上 / info 404 | `tasklist \| findstr play-plugin`，`netstat -ano \| findstr 17653`，config 端口段 |
| 连上但 403 | config.toml `origins` 白名单与页面 Origin（空名单=放行全部） |
| 窗口不出现 | 日志搜 `overlay window created`；没有 → 渲染器失败行；有 → 看 `stream.rect` 是否推送（页面侧） |
| 画面黑/花屏 | `--smoke` 隔离（自带帧内容断言）；软解/硬解切换看 `decoder` 字段 |
| 静默安装后白名单没生效 | 确认 msiexec 命令带了 `ORIGINS="..."`，看 `%APPDATA%\PlayPlugin\config.toml` 时间戳 |
