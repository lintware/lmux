//! JPEG decoding for browser frames, off the UI thread (review #12).
//!
//! The UI thread only enqueues the compressed bytes; one worker decodes with
//! `zune-jpeg` into an RGB `ColorImage`, and the UI drains the result during its
//! next pump. Building straight from RGB avoids the intermediate RGBA buffer the
//! old `image::load_from_memory(...).to_rgba8()` path allocated.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// A decoded frame, ready to upload as a texture.
pub struct DecodedFrame {
    pub surface: String,
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub image: egui::ColorImage,
}

struct Job {
    surface: String,
    seq: u32,
    jpeg: Vec<u8>,
}

pub struct Decoder {
    tx: Sender<Job>,
    rx: Mutex<Receiver<DecodedFrame>>,
    wake: Arc<Mutex<Option<egui::Context>>>,
}

impl Decoder {
    /// A decode worker of its own. Each host has one: a shared queue let one
    /// host drain (and never ack) another host's frames, which stalled its
    /// stream on "waiting for the first frame".
    pub fn new() -> Self {
    let (job_tx, job_rx) = channel::<Job>();
    let (res_tx, res_rx) = channel::<DecodedFrame>();
    let wake: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    let worker_wake = wake.clone();
    std::thread::Builder::new()
        .name("lmux-jpeg".into())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                match decode_jpeg(&job.jpeg) {
                    Some((image, width, height)) => {
                        let _ = res_tx.send(DecodedFrame {
                            surface: job.surface,
                            seq: job.seq,
                            width,
                            height,
                            image,
                        });
                    }
                    None => eprintln!("[frame] jpeg decode failed for {}", job.surface),
                }
                if let Some(ctx) = worker_wake.lock().unwrap().as_ref() {
                    ctx.request_repaint();
                }
            }
        })
        .expect("spawn jpeg decode worker");
        Decoder { tx: job_tx, rx: Mutex::new(res_rx), wake }
    }

    /// Called once by the app so decoded frames wake the UI promptly.
    pub fn set_wake(&self, ctx: egui::Context) {
        *self.wake.lock().unwrap() = Some(ctx);
    }

    pub fn submit(&self, surface: String, seq: u32, jpeg: Vec<u8>) {
        let _ = self.tx.send(Job { surface, seq, jpeg });
    }

    pub fn drain(&self) -> Vec<DecodedFrame> {
        let rx = self.rx.lock().unwrap();
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(f) => out.push(f),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

/// Decode a JPEG into an RGB `ColorImage`. Public so callers (and benchmarks)
/// can use it directly; the worker calls it off the UI thread.
pub fn decode_jpeg(jpeg: &[u8]) -> Option<(egui::ColorImage, u16, u16)> {
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg));
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || pixels.len() < w * h {
        return None;
    }
    let stride = pixels.len() / (w * h);
    let count = w * h;
    let mut out: Vec<egui::Color32> = Vec::with_capacity(count);
    match stride {
        1 => out.extend(pixels[..count].iter().map(|&g| egui::Color32::from_gray(g))),
        4 => out.extend(pixels.chunks_exact(4).take(count).map(|p| {
            egui::Color32::from_rgb(p[0], p[1], p[2])
        })),
        _ => out.extend(pixels.chunks_exact(3).take(count).map(|p| {
            egui::Color32::from_rgb(p[0], p[1], p[2])
        })),
    }
    Some((egui::ColorImage::new([w, h], out), info.width, info.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgb_jpeg() {
        // Encode with the image crate's (already a dev-independent) encoder-less
        // path: build a tiny JPEG via zune's own encoder is not available, so
        // craft one with a known-good minimal baseline JPEG.
        let jpeg = include_bytes!("../tests/data/rgb8.jpg").to_vec();
        let (img, w, h) = decode_jpeg(&jpeg).expect("decode");
        assert_eq!((w, h), (8, 8));
        assert_eq!(img.size, [8, 8]);
        assert_eq!(img.pixels.len(), 64);
    }
}
