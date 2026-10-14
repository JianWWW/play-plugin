#!/bin/bash
# Minimal decode-only FFmpeg configure for play-plugin (MSVC toolchain,
# invoked from build-ffmpeg-min.cmd after vcvars64; MSYS2 /usr/bin on PATH,
# GNU link.exe renamed to gnulink.exe to stop it shadowing MSVC link).
cd "$(dirname "$0")/../.deps/ffmpeg-src" || exit 1
exec ./configure \
  --prefix=/d/code/wangjian/play-plugin/.deps/ffmpeg-min \
  --toolchain=msvc --target-os=win64 --arch=x86_64 \
  --enable-shared --disable-static \
  --disable-everything --disable-programs --disable-doc \
  --disable-avdevice --disable-avfilter --disable-autodetect \
  --enable-avcodec --enable-avformat --enable-avutil --enable-swscale --enable-swresample \
  --enable-decoder=h264,hevc,aac,mp3,pcm_alaw,pcm_mulaw \
  --enable-demuxer=rtsp,rtp,flv,hls,mpegts,mpegps,aac,latm \
  --enable-muxer=mp4 \
  --enable-parser=h264,hevc,aac,aac_latm \
  --enable-protocol=file,tcp,udp,http,https,rtsp,rtp,rtmp,rtmps,tls,crypto \
  --enable-bsf=aac_adtstoasc,h264_mp4toannexb,hevc_mp4toannexb,extract_extradata \
  --enable-d3d11va --enable-dxva2 --enable-schannel \
  --enable-hwaccel=h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2,h264_dxva2,hevc_dxva2 \
