@echo off
REM Minimal FFmpeg build (MSVC + MSYS2): decode-only subset for play-plugin.
REM See configure-ffmpeg-min.sh for the exact feature set.
REM Prereqs: VS2022 BuildTools, MSYS2 (make nasm diffutils), GNU link.exe
REM renamed to gnulink.exe (stops it shadowing MSVC link.exe).
REM Rebuilds from the pristine tarball every run (incremental builds of this
REM tree are unreliable: stale objects kept a pre-D3D11VA hwcontext.o once).
setlocal
cd /d "%~dp0.."

call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 exit /b 1

set PATH=C:\msys64\usr\bin;%PATH%

if not exist .deps\ffmpeg-9.0.2.tar.xz (
  echo [!] missing .deps\ffmpeg-9.0.2.tar.xz - download https://www.ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz
  exit /b 1
)

C:\msys64\usr\bin\bash.exe --noprofile --norc -c "cd .deps && rm -rf ffmpeg-src && tar xf ffmpeg-9.0.2.tar.xz && mv ffmpeg-9.0.2 ffmpeg-src"
if errorlevel 1 (
  echo [!] extract failed
  exit /b 1
)

python installer\patch-configure.py .deps\ffmpeg-src\configure
if errorlevel 1 (
  echo [!] patch failed
  exit /b 1
)

C:\msys64\usr\bin\bash.exe --noprofile --norc installer/configure-ffmpeg-min.sh
if errorlevel 1 (
  echo [!] configure failed
  exit /b 1
)

C:\msys64\usr\bin\bash.exe --noprofile --norc -c "cd .deps/ffmpeg-src && make -j%NUMBER_OF_PROCESSORS% && make install"
if errorlevel 1 (
  echo [!] make failed
  exit /b 1
)

REM MSVC builds drop the import libs next to the DLLs - move them to lib/
move /y .deps\ffmpeg-min\bin\avcodec.lib .deps\ffmpeg-min\lib\ >nul 2>&1
move /y .deps\ffmpeg-min\bin\avformat.lib .deps\ffmpeg-min\lib\ >nul 2>&1
move /y .deps\ffmpeg-min\bin\avutil.lib .deps\ffmpeg-min\lib\ >nul 2>&1
move /y .deps\ffmpeg-min\bin\swresample.lib .deps\ffmpeg-min\lib\ >nul 2>&1
move /y .deps\ffmpeg-min\bin\swscale.lib .deps\ffmpeg-min\lib\ >nul 2>&1

echo [OK] .deps/ffmpeg-min
