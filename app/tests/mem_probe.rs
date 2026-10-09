//! How much memory terminal views take. Informational: run with
//! `cargo test --release --test mem_probe -- --ignored --nocapture`.

use lmux_app::term::TermView;

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap() / 1024.0
}

fn transcript(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        v.extend_from_slice(format!("\x1b[32m{i:05}\x1b[0m lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore\r\n").as_bytes());
    }
    v
}

#[test]
#[ignore]
fn terminal_view_memory() {
    let data = transcript(20_000);
    for (cols, rows) in [(120u16, 40u16), (200, 60)] {
        let before = rss_mb();
        let mut views: Vec<TermView> = Vec::new();
        for _ in 0..10 {
            let mut tv = TermView::new(cols, rows);
            tv.feed(&data);
            views.push(tv);
        }
        let after = rss_mb();
        println!("{cols}x{rows}, 20k lines fed: {:.1} MB per view", (after - before) / 10.0);
        drop(views);
    }
}
