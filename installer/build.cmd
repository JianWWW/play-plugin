@echo off
REM Builds PlayPlugin.msi. Prereqs: WiX Toolset v4 (dotnet tool install --global wix),
REM cargo build --release done, FFmpeg shared DLLs in target\release (see fetch-ffmpeg.ps1).
setlocal
cd /d "%~dp0.."

if not exist target\release\play-plugin.exe (
  echo [!] run: cargo build --release
  exit /b 1
)

REM Stage binaries + DLLs + notices
mkdir installer\staged 2>nul
copy /y target\release\play-plugin.exe installer\staged\ >nul
copy /y .deps\ffmpeg\*\bin\*.dll installer\staged\ >nul
copy /y installer\THIRD-PARTY-NOTICES.txt installer\staged\ >nul

for /f "tokens=2" %%v in ('findstr /r "^version" Cargo.toml') do set PKGVER=%%v
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
