# dev.ps1 — 本地开发调试预览一键脚本
# 用法:
#   .\dev.ps1            构建(debug) + 启动插件 + 启动测试页服务器
#   .\dev.ps1 -Stop      停止插件和测试页服务器
#   .\dev.ps1 -Smoke     只跑 --smoke 自检（无需浏览器）
param([switch]$Stop, [switch]$Smoke)
$ErrorActionPreference = "Continue"
$root = $PSScriptRoot   # dev.ps1 在仓库根目录

function Stop-Dev {
  Get-Process play-plugin -ErrorAction SilentlyContinue | Stop-Process -Force
  Get-CimInstance Win32_Process -Filter "Name='node.exe'" |
    Where-Object { $_.CommandLine -like '*examples*serve.js*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

if ($Stop) { Stop-Dev; Write-Host "已停止。"; exit 0 }

if ($Smoke) {
  cargo run -p plugin-app -- --smoke
  exit $LASTEXITCODE
}

Write-Host "[1/3] 构建 (debug)..."
cargo build -p plugin-app
if ($LASTEXITCODE -ne 0) { Write-Error "构建失败"; exit 1 }

Stop-Dev
Write-Host "[2/3] 启动插件（独立控制台窗口，日志实时可见）..."
Start-Process "$root\target\debug\play-plugin.exe" -WorkingDirectory $root

Write-Host "[3/3] 启动测试页服务器 (127.0.0.1:8099)..."
Start-Process node -ArgumentList "$root\examples\serve.js", "8099" -WorkingDirectory $root -WindowStyle Hidden

# 等插件就绪（FFmpeg/D3D 初始化需要 1~3 秒）
$info = "未响应"
foreach ($i in 1..16) {
  Start-Sleep -Milliseconds 500
  try {
    $info = (Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:17653/info" -TimeoutSec 2).Content
    break
  } catch { $info = "未响应" }
}
Write-Host ""
Write-Host "==== 就绪 ===="
Write-Host "  浏览器打开:  http://127.0.0.1:8099/examples/page/index.html"
Write-Host "  插件探测:    $info"
Write-Host "  插件日志:    独立控制台窗口 + %APPDATA%\PlayPlugin\logs\"
Write-Host '  日志级别:    $env:RUST_LOG=debug 后重跑本脚本可看全量 debug' 
Write-Host "  SDK 改动:    cd sdk && npx tsc --watch，刷新页面生效"
Write-Host "  停止:        .\dev.ps1 -Stop（或托盘菜单 Exit）"
Write-Host "==============="
