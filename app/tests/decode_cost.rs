//! Review #12: JPEG decoding must be off the UI thread. Measures the UI-thread
//! cost of accepting a frame and the worker's decode time for a realistic
//! 1280×800 screencast frame.
//!
//! `cargo test --test decode_cost -- --nocapture` prints the numbers.

use std::time::{Duration, Instant};

use lmux_app::state::AppState;
use lmux_app::wire::{self, Frame};

const FRAME: &[u8] = include_bytes!("data/frame1280.jpg");

fn frame_bytes() -> Vec<u8> {
    Frame::BrowserFrame { surface: "surface:9", seq: 1, width: 1280, height: 800, jpeg: FRAME }
        .encode()
        .unwrap()
}

#[test]
fn ui_thread_accepts_frames_without_decoding() {
    let bytes = frame_bytes();
    let mut st = AppState::new();

    // Warm the worker.
    st.handle_frame(&bytes);
    std::thread::sleep(Duration::from_millis(50));

    let n = 500;
    let t = Instant::now();
    for _ in 0..n {
        st.handle_frame(&bytes);
    }
    let per = t.elapsed() / n;
    println!(
        "handle_frame (UI thread, {} KiB frame): {:?}/frame (submit only)",
        bytes.len() / 1024,
        per
    );
    // The UI thread only copies the bytes and enqueues them.
    assert!(per < Duration::from_millis(1), "UI thread must not decode: {per:?}");
}

#[test]
fn worker_decode_time() {
    // A few iterations; report the best (caches warm).
    let mut best = Duration::MAX;
    for _ in 0..6 {
        let t = Instant::now();
        let (img, w, h) = lmux_app::decode::decode_jpeg(FRAME).expect("decode");
        best = best.min(t.elapsed());
        assert_eq!((w, h), (1280, 800));
        assert_eq!(img.size, [1280, 800]);
    }
    println!("zune-jpeg decode 1280x800 \u{2192} RGB ColorImage: {best:?} (worker thread)");
}

/// End to end: submit a frame, then confirm the worker result is picked up and
/// becomes a drawable image plus an ack.
#[test]
fn frame_round_trips_through_the_worker() {
    let bytes = frame_bytes();
    let mut st = AppState::new();
    st.handle_frame(&bytes);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        st.poll_decoded();
        if st.browsers.get("surface:9").is_some_and(|b| b.have_frame) {
            break;
        }
        assert!(Instant::now() < deadline, "worker did not deliver the frame");
        std::thread::sleep(Duration::from_millis(10));
    }
    let b = st.browsers.get("surface:9").unwrap();
    assert_eq!(b.frame_size, (1280, 800));
    assert_eq!(b.pending_ack, Some(1), "the frame must be acked after decode");
    let _ = wire::KIND_BROWSER_FRAME;
}
