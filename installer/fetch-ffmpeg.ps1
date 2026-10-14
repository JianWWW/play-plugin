# Downloads an FFmpeg full-shared build (headers + MSVC import libs + DLLs)
# into .deps\ffmpeg — FALLBACK for `cargo build` (ffmpeg-sys-next) on machines
# without MSYS2. The shipped MSI always uses the minimal build from
# installer\build-ffmpeg-min.cmd (~13MB vs ~230MB). If you use this fallback,
# point FFMPEG_DIR in .cargo/config.toml at the extracted directory.
# Run from repo root:  powershell -File installer\fetch-ffmpeg.ps1

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path .deps | Out-Null

Write-Host "Downloading FFmpeg shared build (~100MB)..."
Invoke-WebRequest -UseBasicParsing `
  -Uri "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-full-shared.7z" `
  -OutFile ".deps\ffmpeg-shared.7z"

Write-Host "Downloading 7zr.exe (standalone extractor)..."
Invoke-WebRequest -UseBasicParsing -Uri "https://www.7-zip.org/a/7zr.exe" -OutFile ".deps\7zr.exe"

Write-Host "Extracting..."
& .deps\7zr.exe x -y -o.deps\ffmpeg .deps\ffmpeg-shared.7z | Out-Null

Write-Host "Done. Set FFMPEG_DIR to:"
(Get-Item ".deps\ffmpeg\ffmpeg-*full_build-shared").FullName
