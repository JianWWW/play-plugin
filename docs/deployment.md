# 部署手册（企业 IT）

## 安装包

`installer/PlayPlugin.msi`（WiX v4 构建）：

- 默认按用户安装（免 UAC），含开机自启动（HKCU Run）。
- 支持 per-machine 部署；通过 GPO/SCCM 静默安装：

```
msiexec /i PlayPlugin.msi /qn ORIGINS="https://player.example.com;https://intra.example.com"
```

`ORIGINS` 会写入 `%APPDATA%\PlayPlugin\config.toml` 的 `origins` 白名单——这是
本地 WebSocket 的访问控制（不在名单内的网页一律 403）。

## 配置文件

`%APPDATA%\PlayPlugin\config.toml`

```toml
port = 17653              # 监听端口（127.0.0.1），被占顺延 port_fallback 个
port_fallback = 8
max_streams = 32          # 最大并发流数
log_level = "info"        # tracing 过滤器
hardware_decode = true    # D3D11VA 硬解，失败自动回退软解
origins = ["https://player.example.com"]

[update]
enabled = false           # 自动更新（清单 + Authenticode 验签）
manifest_url = "https://your.cdn/play-plugin/manifest.json"
auto_install = false      # true 时验证通过后静默重装
```

## 更新清单格式

```json
{ "version": "1.1.0", "url": "https://your.cdn/play-plugin/PlayPlugin-1.1.0.msi" }
```

插件校验 MSI 的 Authenticode 签名后才允许安装（`auto_install=false` 时仅向页面
广播 `app.updateAvailable`）。

## 卸载

控制面板"卸载"或 `msiexec /x PlayPlugin.msi /qn`：

- 自动结束插件进程（托盘应用直接终止）、移除程序文件、FFmpeg DLL、
  自启动注册表项和 `HKCU\Software\PlayPlugin`。
- **保留** `%APPDATA%\PlayPlugin\`（配置/日志/抓图/崩溃转储）——重装后配置
  自动生效；需要彻底清理请手动删除该目录。

升级 = 新 MSI 静默安装（同 UpgradeCode 自动 MajorUpgrade，进程自动关闭）。

## 数据目录

```
%APPDATA%\PlayPlugin\
  config.toml
  logs\play-plugin.log     # 按天滚动
  snapshots\               # 抓图 PNG
  crash\*.dmp              # 崩溃 minidump
```

## 手动验证

```
play-plugin.exe --smoke
```

打开 4 个测试图案浮层 5 秒后自动退出（验证 D3D 渲染与窗口路径，无需摄像头）。

## 浏览器要求

Chrome / Edge / Firefox 最新两个大版本。https 页面连接本地回环 WebSocket 需要
插件正确响应 PNA 预检（已实现）；新版 Chrome 可能弹出"本地网络访问"权限提示，
用户允许一次即可。

## 已知限制

- 无 GPU 的机器/虚拟机自动回退软解（硬解需要真实 GPU，D3D11VA）。
- HLS 时延受协议本身限制（非 LL-HLS 时 6~30s）。
- 视频区内的网页交互控件需放在占位区外；页面弹层盖住视频时调用 `conceal()`
  （v2 协议已预留 exclusion-zone）。
