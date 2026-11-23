//! End-to-end tests for BlazingDL AntiCompress-style 1× disk streaming unpack:
//! - Remote HTTP Range `.zip` streaming unpack + CRC32 + pause/resume
//! - Remote HTTP `.tar.gz` streaming unpack (zero archive file on disk)
//! - AntiCompress `.acpkg` v1 local & remote HTTP unpack + SHA-256 integrity + free resume

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use blazingdl_extract::stream_unpack::{
    ACPKG_CHUNK_SIZE, ACPKG_MANIFEST_NAME, ACPKG_VERSION, AcChunkInfo, AcFileEntry, AcManifest,
    RESUME_FILE_NAME, StreamUnpackError, parse_acpkg_manifest, serialize_acpkg_manifest,
    unpack_acpkg_local, unpack_url,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

/// Minimal RFC 8878 valid zstd frame wrapping raw uncompressed blocks (max 128 KiB per block).
/// Lets tests produce 100% spec-compliant `.zst` chunks decoded by `ruzstd` without a C encoder.
fn make_raw_zstd_frame(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    // Magic number: 0xFD2FB528 little-endian
    out.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    // Frame Header Descriptor: Single_Segment_Flag=1 (bit 5), FCS_Field_Size=0 (1-byte or 4-byte):
    // Use FCS_Field_Size=2 (bits 7..6 = 10 -> 4-byte Frame_Content_Size) + Single_Segment=1 (bit 5) = 0xA0.
    out.push(0xA0);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    let max_block = 128 * 1024;
    if data.is_empty() {
        // Single empty last raw block: last=1, type=0 (Raw), size=0 -> 3-byte LE header = 1
        out.extend_from_slice(&[0x01, 0x00, 0x00]);
        return out;
    }
    let chunks: Vec<&[u8]> = data.chunks(max_block).collect();
    for (i, ch) in chunks.iter().enumerate() {
        let last = u32::from(i + 1 == chunks.len());
        let header: u32 = last | (0 << 1) | ((ch.len() as u32) << 3);
        let b = header.to_le_bytes();
        out.extend_from_slice(&b[0..3]);
        out.extend_from_slice(ch);
    }
    out
}

/// Spawn a tiny local HTTP/1.1 server supporting `HEAD`, `GET`, and `Range: bytes=start-end`.
async fn spawn_range_server(routes: HashMap<String, Vec<u8>>) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(routes);
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut req_buf = vec![0u8; 8192];
                let Ok(n) = sock.read(&mut req_buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&req_buf[..n]);
                let mut lines = req.lines();
                let first = lines.next().unwrap_or("");
                let mut parts = first.split_whitespace();
                let method = parts.next().unwrap_or("GET");
                let path = parts.next().unwrap_or("/");
                let range_hdr = lines
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned());

                let Some(body) = routes.get(path) else {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                };
                if method == "HEAD" {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    return;
                }
                if let Some(r) = range_hdr.and_then(|v| v.strip_prefix("bytes=").map(str::to_owned))
                    && let Some((s, e)) = r.split_once('-')
                    && let Ok(start) = s.parse::<usize>()
                {
                    let end = e
                        .parse::<usize>()
                        .unwrap_or_else(|_| body.len().saturating_sub(1))
                        .min(body.len().saturating_sub(1));
                    let slice = &body[start..=end];
                    let hdr = format!(
                        "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        slice.len()
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(slice).await;
                    return;
                }
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(hdr.as_bytes()).await;
                let _ = sock.write_all(body).await;
            });
        }
    });
    (Url::parse(&format!("http://{addr}/")).unwrap(), handle)
}

fn build_test_zip(entries: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        for (name, data, method) in entries {
            let opts = zip::write::SimpleFileOptions::default().compression_method(*method);
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

#[tokio::test]
async fn streams_remote_zip_over_range_and_resumes_cleanly() {
    let big_a = vec![b'A'; 48 * 1024];
    let big_b = (0..32_768).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let zip_bytes = build_test_zip(&[
        ("game/bin/engine.dll", &big_a, zip::CompressionMethod::Deflated),
        ("game/readme.txt", b"BlazingDL 1x disk unpack!", zip::CompressionMethod::Stored),
        ("game/assets/map.dat", &big_b, zip::CompressionMethod::Deflated),
    ]);

    let mut routes = HashMap::new();
    routes.insert("/MyGame.zip".to_owned(), zip_bytes.clone());
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let url = base.join("MyGame.zip").unwrap();
    let report = unpack_url(&client, &url, dest.path(), 4, Arc::new(AtomicBool::new(false)))
        .await
        .expect("stream zip unpack succeeds");

    assert_eq!(report.files, 3);
    assert_eq!(report.skipped_resumed, 0);
    assert!(!dest.path().join("MyGame.zip").exists(), "archive never written to disk");
    assert!(!dest.path().join(RESUME_FILE_NAME).exists(), "resume state cleared on finish");
    assert_eq!(std::fs::read(dest.path().join("game/bin/engine.dll")).unwrap(), big_a);
    assert_eq!(
        std::fs::read(dest.path().join("game/readme.txt")).unwrap(),
        b"BlazingDL 1x disk unpack!"
    );
    assert_eq!(std::fs::read(dest.path().join("game/assets/map.dat")).unwrap(), big_b);

    // Simulate interrupted resume: keep readme.txt + resume state listing it completed, delete others.
    std::fs::remove_file(dest.path().join("game/bin/engine.dll")).unwrap();
    std::fs::remove_file(dest.path().join("game/assets/map.dat")).unwrap();
    std::fs::write(
        dest.path().join(RESUME_FILE_NAME),
        format!(
            r#"{{"total_size":{},"completed":["game/readme.txt"]}}"#,
            zip_bytes.len()
        ),
    )
    .unwrap();
    let resumed = unpack_url(&client, &url, dest.path(), 4, Arc::new(AtomicBool::new(false)))
        .await
        .expect("resumed stream zip unpack succeeds");
    assert_eq!(resumed.skipped_resumed, 1);
    assert_eq!(resumed.files, 2);
    assert_eq!(std::fs::read(dest.path().join("game/bin/engine.dll")).unwrap(), big_a);

    srv.abort();
}

#[tokio::test]
async fn streams_remote_tar_gz_directly_to_folder() {
    let mut gz_buf = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut gz_buf, flate2::Compression::fast());
        let mut builder = tar::Builder::new(enc);
        let payload = b"streaming tar.gz payload - zero archive file on disk";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "pkg/hello.txt", &payload[..])
            .unwrap();
        let enc = builder.into_inner().unwrap();
        enc.finish().unwrap();
    }

    let mut routes = HashMap::new();
    routes.insert("/bundle.tar.gz".to_owned(), gz_buf);
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let url = base.join("bundle.tar.gz").unwrap();
    let report = unpack_url(&client, &url, dest.path(), 4, Arc::new(AtomicBool::new(false)))
        .await
        .expect("tar.gz stream unpack succeeds");

    assert_eq!(report.files, 1);
    assert!(!dest.path().join("bundle.tar.gz").exists());
    assert_eq!(
        std::fs::read(dest.path().join("pkg/hello.txt")).unwrap(),
        b"streaming tar.gz payload - zero archive file on disk"
    );

    srv.abort();
}

#[tokio::test]
async fn acpkg_v1_verifies_sha256_unpacks_local_and_remote_and_resumes_free() {
    let f1 = b"first file inside acpkg!".to_vec();
    let f2 = vec![0x5Au8; 64 * 1024];
    let mut stream = Vec::new();
    stream.extend_from_slice(&f1);
    stream.extend_from_slice(&f2);

    let chunk0_zst = make_raw_zstd_frame(&stream);
    let chunk0_sha = hex::encode(Sha256::digest(&chunk0_zst));

    let manifest = AcManifest {
        version: ACPKG_VERSION,
        chunk_size: ACPKG_CHUNK_SIZE,
        total_size: stream.len() as u64,
        files: vec![
            AcFileEntry {
                path: "sub/one.txt".into(),
                size: f1.len() as u64,
                offset: 0,
                sha256: hex::encode(Sha256::digest(&f1)),
            },
            AcFileEntry {
                path: "data/two.bin".into(),
                size: f2.len() as u64,
                offset: f1.len() as u64,
                sha256: hex::encode(Sha256::digest(&f2)),
            },
        ],
        chunks: vec![AcChunkInfo {
            index: 0,
            sha256: chunk0_sha,
        }],
        manifest_sha256: String::new(),
    };
    let json = serialize_acpkg_manifest(&manifest);
    let parsed = parse_acpkg_manifest(&json).expect("canonical manifest self-hash valid");
    assert_eq!(parsed.files.len(), 2);

    // Tampered manifest must be rejected!
    let tampered = json.replace("sub/one.txt", "sub/bad.txt");
    assert!(matches!(
        parse_acpkg_manifest(&tampered),
        Err(StreamUnpackError::Integrity(_))
    ));

    // 1. Local .acpkg install with delete_chunks = true (1x disk space).
    let pkg_dir = tempfile::tempdir().unwrap();
    std::fs::write(pkg_dir.path().join(ACPKG_MANIFEST_NAME), &json).unwrap();
    let chunk_path = pkg_dir.path().join(AcManifest::chunk_name(0));
    std::fs::write(&chunk_path, &chunk0_zst).unwrap();

    let local_dest = tempfile::tempdir().unwrap();
    let local_rep = unpack_acpkg_local(
        pkg_dir.path(),
        local_dest.path(),
        true,
        &AtomicBool::new(false),
    )
    .expect("local acpkg unpack succeeds");
    assert_eq!(local_rep.files, 2);
    assert!(!chunk_path.exists(), "chunk deleted after consumption");
    assert_eq!(std::fs::read(local_dest.path().join("sub/one.txt")).unwrap(), f1);
    assert_eq!(std::fs::read(local_dest.path().join("data/two.bin")).unwrap(), f2);

    // 2. Remote HTTP .acpkg streaming unpack + free resume check.
    let mut routes = HashMap::new();
    routes.insert("/game.acpkg/manifest.json".to_owned(), json.into_bytes());
    routes.insert("/game.acpkg/chunk-000000.zst".to_owned(), chunk0_zst);
    let (base, srv) = spawn_range_server(routes).await;

    let remote_dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let pkg_url = base.join("game.acpkg").unwrap();
    let rep1 = unpack_url(&client, &pkg_url, remote_dest.path(), 4, Arc::new(AtomicBool::new(false)))
        .await
        .expect("remote acpkg unpack succeeds");
    assert_eq!(rep1.files, 2);
    assert!(rep1.bytes_downloaded > 0);
    assert_eq!(std::fs::read(remote_dest.path().join("sub/one.txt")).unwrap(), f1);
    assert_eq!(std::fs::read(remote_dest.path().join("data/two.bin")).unwrap(), f2);

    // Re-running into same destination skips both verified files with 0 chunk bytes downloaded!
    let rep2 = unpack_url(&client, &pkg_url, remote_dest.path(), 4, Arc::new(AtomicBool::new(false)))
        .await
        .expect("resumed remote acpkg succeeds");
    assert_eq!(rep2.skipped_resumed, 2);
    assert_eq!(rep2.bytes_downloaded, 0);

    srv.abort();
}
