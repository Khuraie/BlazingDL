//! Reproducible live 1× disk streaming unpack benchmark.
//!
//! Usage:
//!   cargo run --release -p blazingdl-extract --example live_unpack -- <url> <out_dir> [concurrency]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use blazingdl_extract::stream_unpack::unpack_url;
use url::Url;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: live_unpack <url> <out_dir> [concurrency=8]");
        std::process::exit(2);
    }
    let url = Url::parse(&args[0]).expect("valid URL");
    let dest = PathBuf::from(&args[1]);
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);

    let client = reqwest::Client::builder()
        .user_agent("BlazingDL-bench")
        .build()
        .expect("http client");

    let start = Instant::now();
    let report = unpack_url(
        &client,
        &url,
        &dest,
        concurrency,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("stream unpack failed");
    let secs = start.elapsed().as_secs_f64();
    let dl_mib = report.bytes_downloaded as f64 / (1024.0 * 1024.0);
    let out_mib = report.bytes_written as f64 / (1024.0 * 1024.0);
    let speed = if secs > 0.0 { dl_mib / secs } else { 0.0 };

    println!("url:              {url}");
    println!("files:            {}", report.files);
    println!(
        "bytes_downloaded: {} ({dl_mib:.2} MiB)",
        report.bytes_downloaded
    );
    println!(
        "bytes_written:    {} ({out_mib:.2} MiB)",
        report.bytes_written
    );
    println!("elapsed_sec:      {secs:.2}");
    println!("avg_wire_mib_s:   {speed:.2}");
    println!("dest_dir:         {}", report.dest_dir.display());
}
