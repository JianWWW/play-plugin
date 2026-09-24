//! WASAPI shared-mode audio sinks. Each unmuted stream gets its own sink;
//! the Windows mixer combines them. Format: f32 / 48 kHz / stereo in, WASAPI
//! auto-converts to the device mix format.

use plugin_core::AudioChunk;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BUFFER_HNS: i64 = 500_000; // 50 ms (100ns units)
const MAX_RING: usize = 48_000; // ~0.5 s stereo samples before dropping

struct SinkHandle {
    cancel: Arc<AtomicBool>,
    ring: Arc<Mutex<VecDeque<f32>>>,
}

#[derive(Default)]
pub struct AudioHub {
    sinks: Mutex<HashMap<u64, SinkHandle>>,
}

impl AudioHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a decoded audio chunk; starts a WASAPI sink on first audio.
    pub fn push(&self, stream_id: u64, chunk: &AudioChunk) {
        let mut sinks = self.sinks.lock().unwrap();
        let handle = sinks.entry(stream_id).or_insert_with(|| {
            let cancel = Arc::new(AtomicBool::new(false));
            let ring = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_RING)));
            start_sink(cancel.clone(), ring.clone());
            SinkHandle { cancel, ring }
        });
        let mut ring = handle.ring.lock().unwrap();
        for s in chunk.samples.iter() {
            let cap = MAX_RING;
            if ring.len() >= cap {
                let keep_from = cap / 4;
                ring.drain(..keep_from); // shed backlog, keep latency bounded
            }
            ring.push_back(*s);
        }
    }

    pub fn drop_stream(&self, stream_id: u64) {
        if let Some(h) = self.sinks.lock().unwrap().remove(&stream_id) {
            h.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn drop_all(&self) {
        let mut sinks = self.sinks.lock().unwrap();
        for (_, h) in sinks.drain() {
            h.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn start_sink(cancel: Arc<AtomicBool>, ring: Arc<Mutex<VecDeque<f32>>>) {
    let _ = std::thread::Builder::new()
        .name("audio-sink".into())
        .spawn(move || unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                return;
            }
            let run = sink_main(&cancel, &ring);
            if let Err(e) = run {
                tracing::warn!(error = %e, "audio sink exited");
            }
        });
}

fn sink_main(cancel: &AtomicBool, ring: &Mutex<VecDeque<f32>>) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(
            windows::Win32::Media::Audio::eRender,
            windows::Win32::Media::Audio::eConsole,
        )?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let wf = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: SAMPLE_RATE,
            wBitsPerSample: 32,
            nBlockAlign: (CHANNELS * 4),
            nAvgBytesPerSec: SAMPLE_RATE * (CHANNELS * 4) as u32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_HNS,
            0,
            &wf,
            None,
        )?;
        let render: IAudioRenderClient = client.GetService()?;
        client.Start()?;

        let frame_bytes = (CHANNELS as u32) * 4;
        let mut buf = vec![0f32; 4096];
        while !cancel.load(Ordering::Relaxed) {
            let padding = client.GetCurrentPadding()?;
            let buffer_frames = ((BUFFER_HNS as u64 * SAMPLE_RATE as u64) / 10_000_000) as u32;
            let avail = buffer_frames.saturating_sub(padding);
            if avail == 0 {
                std::thread::sleep(std::time::Duration::from_millis(4));
                continue;
            }
            let want = (avail as usize).min(buf.len() / CHANNELS as usize);
            let have = {
                let mut r = ring.lock().unwrap();
                let have = want.min(r.len() / CHANNELS as usize);
                for slot in buf.iter_mut().take(have * CHANNELS as usize) {
                    *slot = r.pop_front().unwrap_or(0.0);
                }
                have
            };
            if have == 0 {
                // Underrun: write silence briefly so the stream stays alive.
                std::thread::sleep(std::time::Duration::from_millis(4));
                continue;
            }
            let data_ptr = render.GetBuffer(have as u32)?;
            let dst =
                std::slice::from_raw_parts_mut(data_ptr, have as usize * frame_bytes as usize);
            for (i, sample) in dst.chunks_exact_mut(4).enumerate() {
                sample.copy_from_slice(&buf[i].to_le_bytes());
            }
            render.ReleaseBuffer(have as u32, 0)?;
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        let _ = client.Stop();
        Ok(())
    }
}
