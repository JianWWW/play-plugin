@echo off
REM Builds PlayPlugin.msi. Prereqs: WiX Toolset v4 (dotnet tool install --global wix),
REM cargo build --release done, FFmpeg shared DLLs in target\release (see fetch-ffmpeg.ps1).
setlocal
cd /d "%~dp0.."

if not exist target\release\play-plugin.exe (
  echo [!] run: cargo build --release
  exit /b 1
)

REM Stage binaries + DLLs + notices (minimal FFmpeg build, see .deps\build-ffmpeg-min.cmd)
mkdir installer\staged 2>nul
if not exist .deps\ffmpeg-min\bin\avcodec-63.dll (
  echo [!] missing .deps\ffmpeg-min - run .deps\build-ffmpeg-min.cmd first
  exit /b 1
)
copy /y target\release\play-plugin.exe installer\staged\ >nul
for %%d in (avcodec-63 avformat-63 avutil-61 swresample-7 swscale-10) do copy /y .deps\ffmpeg-min\bin\%%d.dll installer\staged\ >nul
copy /y installer\THIRD-PARTY-NOTICES.txt installer\staged\ >nul

for /f "tokens=3" %%v in ('findstr /b /c:"version = " Cargo.toml') do set PKGVER=%%v
set PKGVER=%PKGVER:"=%

wix eula accept wix7 >nul 2>&1

wix build -arch x64 ^
  -ext WixToolset.Util.wixext ^
  -define PkgVersion=%PKGVER% ^
  -define BinDir="%~dp0staged" ^
  -out installer\PlayPlugin.msi ^
  installer\PlayPlugin.wxs || exit /b 1

REM Signing (set SIGNTOOL_PFX / SIGNTOOL_PASS in CI secrets):
REM signtool sign /fd SHA256 /f "%SIGNTOOL_PFX%" /p "%SIGNTOOL_PASS%" installer\PlayPlugin.msi

echo [OK] installer\PlayPlugin.msi
