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
        let header: u32 = last | ((ch.len() as u32) << 3);
        let b = header.to_le_bytes();
        out.extend_from_slice(&b[0..3]);
        out.extend_from_slice(ch);
    }
    out
}

/// Spawn a tiny local HTTP/1.1 server supporting `HEAD`, `GET`, and `Range: bytes=start-end`.
async fn spawn_range_server(
    routes: HashMap<String, Vec<u8>>,
) -> (Url, tokio::task::JoinHandle<()>) {
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
        (
            "game/bin/engine.dll",
            &big_a,
            zip::CompressionMethod::Deflated,
        ),
        (
            "game/readme.txt",
            b"BlazingDL 1x disk unpack!",
            zip::CompressionMethod::Stored,
        ),
        (
            "game/assets/map.dat",
            &big_b,
            zip::CompressionMethod::Deflated,
        ),
    ]);

    let mut routes = HashMap::new();
    routes.insert("/MyGame.zip".to_owned(), zip_bytes.clone());
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let url = base.join("MyGame.zip").unwrap();
    let report = unpack_url(
        &client,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("stream zip unpack succeeds");

    assert_eq!(report.files, 3);
    assert_eq!(report.skipped_resumed, 0);
    assert!(
        !dest.path().join("MyGame.zip").exists(),
        "archive never written to disk"
    );
    assert!(
        !dest.path().join(RESUME_FILE_NAME).exists(),
        "resume state cleared on finish"
    );
    assert_eq!(
        std::fs::read(dest.path().join("game/bin/engine.dll")).unwrap(),
        big_a
    );
    assert_eq!(
        std::fs::read(dest.path().join("game/readme.txt")).unwrap(),
        b"BlazingDL 1x disk unpack!"
    );
    assert_eq!(
        std::fs::read(dest.path().join("game/assets/map.dat")).unwrap(),
        big_b
    );

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
    let resumed = unpack_url(
        &client,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("resumed stream zip unpack succeeds");
    assert_eq!(resumed.skipped_resumed, 1);
    assert_eq!(resumed.files, 2);
    assert_eq!(
        std::fs::read(dest.path().join("game/bin/engine.dll")).unwrap(),
        big_a
    );

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
    let report = unpack_url(
        &client,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
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
    assert_eq!(
        std::fs::read(local_dest.path().join("sub/one.txt")).unwrap(),
        f1
    );
    assert_eq!(
        std::fs::read(local_dest.path().join("data/two.bin")).unwrap(),
        f2
    );

    // 2. Remote HTTP .acpkg streaming unpack + free resume check.
    let mut routes = HashMap::new();
    routes.insert("/game.acpkg/manifest.json".to_owned(), json.into_bytes());
    routes.insert("/game.acpkg/chunk-000000.zst".to_owned(), chunk0_zst);
    let (base, srv) = spawn_range_server(routes).await;

    let remote_dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let pkg_url = base.join("game.acpkg").unwrap();
    let rep1 = unpack_url(
        &client,
        &pkg_url,
        remote_dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("remote acpkg unpack succeeds");
    assert_eq!(rep1.files, 2);
    assert!(rep1.bytes_downloaded > 0);
    assert_eq!(
        std::fs::read(remote_dest.path().join("sub/one.txt")).unwrap(),
        f1
    );
    assert_eq!(
        std::fs::read(remote_dest.path().join("data/two.bin")).unwrap(),
        f2
    );

    // Re-running into same destination skips both verified files with 0 chunk bytes downloaded!
    let rep2 = unpack_url(
        &client,
        &pkg_url,
        remote_dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("resumed remote acpkg succeeds");
    assert_eq!(rep2.skipped_resumed, 2);
    assert_eq!(rep2.bytes_downloaded, 0);

    srv.abort();
}

/// Build a synthetic sparse virtual ZIP64 archive whose total size exceeds 320 GiB (`320 << 30`),
/// placing a real stored entry + Central Directory + ZIP64 EOCD + ZIP64 Locator past 320 GiB so
/// we prove 64-bit offsets, ZIP64 extra-field decoding, and zero-OOM streaming over >300 GiB!
#[tokio::test]
async fn streams_zip64_over_320gib_virtual_offsets_without_oom() {
    let base_offset: u64 = 320u64 << 30; // 320 GiB virtual offset (> 300 GB!)
    let name = b"mega/pak001.bin";
    let payload: Vec<u8> = (0..96 * 1024).map(|i| ((i * 31) % 251) as u8).collect();
    let crc = crc32fast::hash(&payload);

    let mut lfh_and_data = Vec::new();
    lfh_and_data.extend_from_slice(b"PK\x03\x04");
    lfh_and_data.extend_from_slice(&45u16.to_le_bytes());
    lfh_and_data.extend_from_slice(&0u16.to_le_bytes());
    lfh_and_data.extend_from_slice(&0u16.to_le_bytes());
    lfh_and_data.extend_from_slice(&0u16.to_le_bytes());
    lfh_and_data.extend_from_slice(&0u16.to_le_bytes());
    lfh_and_data.extend_from_slice(&crc.to_le_bytes());
    lfh_and_data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    lfh_and_data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    lfh_and_data.extend_from_slice(&(name.len() as u16).to_le_bytes());
    lfh_and_data.extend_from_slice(&0u16.to_le_bytes());
    lfh_and_data.extend_from_slice(name);
    lfh_and_data.extend_from_slice(&payload);

    let cd_offset = base_offset + lfh_and_data.len() as u64;

    let mut extra = Vec::new();
    extra.extend_from_slice(&0x0001u16.to_le_bytes());
    extra.extend_from_slice(&8u16.to_le_bytes());
    extra.extend_from_slice(&base_offset.to_le_bytes());

    let mut cd = Vec::new();
    cd.extend_from_slice(b"PK\x01\x02");
    cd.extend_from_slice(&45u16.to_le_bytes());
    cd.extend_from_slice(&45u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&crc.to_le_bytes());
    cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    cd.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
    cd.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u16.to_le_bytes());
    cd.extend_from_slice(&0u32.to_le_bytes());
    cd.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    cd.extend_from_slice(name);
    cd.extend_from_slice(&extra);

    let z64_eocd_offset = cd_offset + cd.len() as u64;
    let mut z64_eocd = Vec::new();
    z64_eocd.extend_from_slice(b"PK\x06\x06");
    z64_eocd.extend_from_slice(&44u64.to_le_bytes());
    z64_eocd.extend_from_slice(&45u16.to_le_bytes());
    z64_eocd.extend_from_slice(&45u16.to_le_bytes());
    z64_eocd.extend_from_slice(&0u32.to_le_bytes());
    z64_eocd.extend_from_slice(&0u32.to_le_bytes());
    z64_eocd.extend_from_slice(&1u64.to_le_bytes());
    z64_eocd.extend_from_slice(&1u64.to_le_bytes());
    z64_eocd.extend_from_slice(&(cd.len() as u64).to_le_bytes());
    z64_eocd.extend_from_slice(&cd_offset.to_le_bytes());

    let mut z64_loc = Vec::new();
    z64_loc.extend_from_slice(b"PK\x06\x07");
    z64_loc.extend_from_slice(&0u32.to_le_bytes());
    z64_loc.extend_from_slice(&z64_eocd_offset.to_le_bytes());
    z64_loc.extend_from_slice(&1u32.to_le_bytes());

    let mut eocd = Vec::new();
    eocd.extend_from_slice(b"PK\x05\x06");
    eocd.extend_from_slice(&0u16.to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes());
    eocd.extend_from_slice(&1u16.to_le_bytes());
    eocd.extend_from_slice(&1u16.to_le_bytes());
    eocd.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    eocd.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes());

    let mut tail_region = Vec::new();
    tail_region.extend_from_slice(&lfh_and_data);
    tail_region.extend_from_slice(&cd);
    tail_region.extend_from_slice(&z64_eocd);
    tail_region.extend_from_slice(&z64_loc);
    tail_region.extend_from_slice(&eocd);

    let virtual_total = base_offset + tail_region.len() as u64;
    assert!(virtual_total > 320u64 << 30);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let region = Arc::new(tail_region);
    let srv = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let region = region.clone();
            tokio::spawn(async move {
                let mut req_buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut req_buf).await else {
                    return;
                };
                let req = String::from_utf8_lossy(&req_buf[..n]);
                let first = req.lines().next().unwrap_or("");
                let method = first.split_whitespace().next().unwrap_or("GET");
                let range_hdr = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned());
                if method == "HEAD" {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {virtual_total}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    return;
                }
                if let Some(r) = range_hdr.and_then(|v| v.strip_prefix("bytes=").map(str::to_owned))
                    && let Some((s, e)) = r.split_once('-')
                    && let Ok(start) = s.parse::<u64>()
                {
                    let end = e
                        .parse::<u64>()
                        .unwrap_or(virtual_total - 1)
                        .min(virtual_total - 1);
                    let len = (end - start + 1) as usize;
                    let mut out = vec![0u8; len];
                    if end >= base_offset {
                        let ov_start = start.max(base_offset);
                        let dst_off = (ov_start - start) as usize;
                        let src_off = (ov_start - base_offset) as usize;
                        let copy_n = ((end - ov_start + 1) as usize).min(region.len() - src_off);
                        out[dst_off..dst_off + copy_n]
                            .copy_from_slice(&region[src_off..src_off + copy_n]);
                    }
                    let hdr = format!(
                        "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{virtual_total}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(hdr.as_bytes()).await;
                    let _ = sock.write_all(&out).await;
                }
            });
        }
    });

    let dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let url = Url::parse(&format!("http://{addr}/HugeGame320GB.zip")).unwrap();
    let rep = unpack_url(
        &client,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("320+ GiB ZIP64 stream unpack succeeds without OOM");

    assert_eq!(rep.files, 1);
    assert_eq!(rep.bytes_written, payload.len() as u64);
    assert_eq!(
        std::fs::read(dest.path().join("mega/pak001.bin")).unwrap(),
        payload
    );

    srv.abort();
}

#[tokio::test]
async fn zip_store_mid_entry_pause_and_resume_reuses_acpart() {
    let payload: Vec<u8> = (0..128 * 1024).map(|i| ((i * 17) % 251) as u8).collect();
    let zip_bytes =
        build_test_zip(&[("big/archive.iso", &payload, zip::CompressionMethod::Stored)]);

    let mut routes = HashMap::new();
    routes.insert("/IsoPack.zip".to_owned(), zip_bytes);
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    let half = 64 * 1024;
    let part_dir = dest.path().join("big");
    std::fs::create_dir_all(&part_dir).unwrap();
    std::fs::write(part_dir.join("archive.iso.acpart"), &payload[..half]).unwrap();

    let client = reqwest::Client::new();
    let url = base.join("IsoPack.zip").unwrap();
    let rep = unpack_url(
        &client,
        &url,
        dest.path(),
        2,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("mid-entry STORE resume succeeds");

    assert_eq!(rep.files, 1);
    assert_eq!(
        rep.bytes_written,
        (payload.len() - half) as u64,
        "only remaining half should be written on mid-entry resume"
    );
    assert!(
        !part_dir.join("archive.iso.acpart").exists(),
        ".acpart cleaned up after atomic rename"
    );
    assert_eq!(
        std::fs::read(part_dir.join("archive.iso")).unwrap(),
        payload
    );

    srv.abort();
}

#[tokio::test]
async fn plain_tar_pauses_and_resumes_via_http_range() {
    let f1 = b"first tar file contents - already unpacked before pause";
    let f2 = b"second tar file contents - fetched after HTTP Range resume";
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (name, data) in [("dir/one.txt", &f1[..]), ("dir/two.txt", &f2[..])] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, name, data).unwrap();
        }
        builder.finish().unwrap();
    }

    let mut routes = HashMap::new();
    routes.insert("/pack.tar".to_owned(), tar_bytes.clone());
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dest.path().join("dir")).unwrap();
    std::fs::write(dest.path().join("dir/one.txt"), f1).unwrap();
    std::fs::write(
        dest.path().join(RESUME_FILE_NAME),
        format!(
            r#"{{"total_size":{},"next_offset":1024,"files_done":1,"bytes_written":{}}}"#,
            tar_bytes.len(),
            f1.len()
        ),
    )
    .unwrap();

    let client = reqwest::Client::new();
    let url = base.join("pack.tar").unwrap();
    let rep = unpack_url(
        &client,
        &url,
        dest.path(),
        1,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("resumed plain tar unpack succeeds");

    assert_eq!(rep.skipped_resumed, 1);
    assert_eq!(rep.files, 1);
    assert!(
        rep.bytes_downloaded < tar_bytes.len() as u64,
        "Range resume must skip already-unpacked prefix"
    );
    assert_eq!(std::fs::read(dest.path().join("dir/two.txt")).unwrap(), f2);

    srv.abort();
}

#[tokio::test]
async fn multi_member_tar_gz_streams_all_members() {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        for (name, data) in [("m/a.txt", &b"member-a"[..]), ("m/b.txt", &b"member-b"[..])] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, name, data).unwrap();
        }
        builder.finish().unwrap();
    }
    let mid = tar_bytes.len() / 2;
    let mut multi_gz = Vec::new();
    for slice in [&tar_bytes[..mid], &tar_bytes[mid..]] {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(slice).unwrap();
        multi_gz.extend_from_slice(&enc.finish().unwrap());
    }

    let mut routes = HashMap::new();
    routes.insert("/multi.tar.gz".to_owned(), multi_gz);
    let (base, srv) = spawn_range_server(routes).await;

    let dest = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let url = base.join("multi.tar.gz").unwrap();
    let rep = unpack_url(
        &client,
        &url,
        dest.path(),
        2,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .expect("multi-member tar.gz unpack succeeds");

    assert_eq!(rep.files, 2);
    assert_eq!(
        std::fs::read(dest.path().join("m/a.txt")).unwrap(),
        b"member-a"
    );
    assert_eq!(
        std::fs::read(dest.path().join("m/b.txt")).unwrap(),
        b"member-b"
    );

    srv.abort();
}

#[test]
fn limits_scale_for_350gib_and_local_tar_zst_extracts_to_folder() {
    let lim_350 = blazingdl_extract::Limits::for_archive(350u64 << 30);
    assert!(lim_350.total >= 350u64 << 30);
    assert_eq!(lim_350.entries, 2_000_000);

    let lim_tiny = blazingdl_extract::Limits::for_archive(1024);
    assert_eq!(lim_tiny.entries, 100_000);

    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let data = b"local zstd tarball content!";
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder
            .append_data(&mut h, "inner/file.txt", &data[..])
            .unwrap();
        builder.finish().unwrap();
    }
    let zst_bytes = make_raw_zstd_frame(&tar_bytes);
    let tmp = tempfile::tempdir().unwrap();
    let arch_path = tmp.path().join("bundle.tar.zst");
    std::fs::write(&arch_path, &zst_bytes).unwrap();

    let mut noop = |_: &std::path::Path| {};
    let mut policy = blazingdl_extract::Policy {
        dest: tmp.path().to_path_buf(),
        subfolder: blazingdl_extract::Subfolder::Always,
        password: None,
        limits: blazingdl_extract::Limits::for_archive(zst_bytes.len() as u64),
        tool: None,
        hatchzip: None,
        in_place: false,
        runs: None,
        on_file: &mut noop,
        cancel: None,
    };
    let report = blazingdl_extract::extract(&arch_path, &mut policy).expect("tar.zst extracts");
    assert_eq!(report.files, 1);
    assert_eq!(
        std::fs::read(tmp.path().join("bundle/inner/file.txt")).unwrap(),
        b"local zstd tarball content!"
    );
}

#[tokio::test]
async fn parallel_store_multi_connection_pause_resume_and_crc32_combine() {
    let payload: Vec<u8> = (0..1_536 * 1024)
        .map(|i| ((i * 29 + (i >> 7) * 13) % 251) as u8)
        .collect();
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut zip_buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("big/asset.pak", opts).unwrap();
        zw.write_all(&payload).unwrap();
        zw.finish().unwrap();
    }
    let zip_bytes = Arc::new(zip_buf.into_inner());

    // Throttled Range server so pause reliably triggers mid-transfer across 4 parallel slices.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_srv = zip_bytes.clone();
    let srv = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let data = data_srv.clone();
            tokio::spawn(async move {
                let mut req_buf = vec![0u8; 4096];
                let Ok(n) = stream.read(&mut req_buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let text = String::from_utf8_lossy(&req_buf[..n]);
                let method = text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("GET");
                let range = text.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("range")
                        .then(|| v.trim().to_owned())
                });
                let total = data.len();
                if method == "HEAD" {
                    let h = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(h.as_bytes()).await;
                    return;
                }
                let (s, e) = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.split_once('-'))
                    .map(|(a, b)| {
                        let s = a.parse::<usize>().unwrap_or(0);
                        let e = b.parse::<usize>().unwrap_or(total - 1).min(total - 1);
                        (s, e)
                    })
                    .unwrap_or((0, total - 1));
                let slice = &data[s..=e];
                let hdr = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                if stream.write_all(hdr.as_bytes()).await.is_err() {
                    return;
                }
                for chunk in slice.chunks(32 * 1024) {
                    if stream.write_all(chunk).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(8)).await;
                }
            });
        }
    });

    let url = Url::parse(&format!("http://{addr}/game.zip")).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut http = blazingdl_extract::UnpackHttp::single(reqwest::Client::new());
    http.parallel_store_threshold = 256 * 1024; // trigger 4-slice parallel STORE on 1.5 MiB file

    // 1. Start and pause mid-flight.
    let cancel1 = Arc::new(AtomicBool::new(false));
    let prog1 = Arc::new(blazingdl_extract::UnpackProgress::default());
    let cancel_trig = cancel1.clone();
    let prog_watch = prog1.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if prog_watch
                .done_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 350 * 1024
            {
                cancel_trig.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });
    let err = blazingdl_extract::stream_unpack::unpack_url_with_http(
        &http,
        &url,
        dest.path(),
        4,
        cancel1,
        Some(prog1),
    )
    .await
    .expect_err("first run must cancel cleanly");
    assert!(matches!(
        err,
        blazingdl_extract::StreamUnpackError::Cancelled
    ));

    // Verify resume JSON saved parallel `store_partials` checkpoints!
    let resume_json = std::fs::read_to_string(dest.path().join(".anticompress-resume.json"))
        .expect("resume file persisted on pause");
    assert!(
        resume_json.contains("store_partials"),
        "expected store_partials in resume JSON: {resume_json}"
    );
    assert!(dest.path().join("big/asset.pak.acpart").exists());

    // 2. Instant O(1) Resume across 4 parallel streams + exact CRC32 combine check!
    let prog2 = Arc::new(blazingdl_extract::UnpackProgress::default());
    let rep = blazingdl_extract::stream_unpack::unpack_url_with_http(
        &http,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
        Some(prog2),
    )
    .await
    .expect("parallel STORE resume must succeed with exact CRC32");
    assert_eq!(rep.files, 1);
    assert!(
        rep.bytes_written < payload.len() as u64,
        "resumed run should write only remaining bytes, wrote {}",
        rep.bytes_written
    );
    assert_eq!(
        std::fs::read(dest.path().join("big/asset.pak")).unwrap(),
        payload
    );
    assert!(!dest.path().join(".anticompress-resume.json").exists());
    srv.abort();
}

#[tokio::test]
async fn retries_503_and_mid_stream_drop_with_headers_hint_and_multi_nic() {
    let payload: Vec<u8> = (0..512 * 1024)
        .map(|i| ((i * 19 + 7) % 251) as u8)
        .collect();
    let mut zip_buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut zip_buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("signed/data.bin", opts).unwrap();
        zw.write_all(&payload).unwrap();
        zw.finish().unwrap();
    }
    let zip_bytes = Arc::new(zip_buf.into_inner());
    let req_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dropped_once = Arc::new(AtomicBool::new(false));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_srv = zip_bytes.clone();
    let cnt_srv = req_counter.clone();
    let drop_srv = dropped_once.clone();
    let srv = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let data = data_srv.clone();
            let cnt = cnt_srv.clone();
            let dropped = drop_srv.clone();
            tokio::spawn(async move {
                let mut req_buf = vec![0u8; 4096];
                let Ok(n) = stream.read(&mut req_buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let text = String::from_utf8_lossy(&req_buf[..n]);
                // Enforce Cookie + Authorization header forwarding!
                let has_cookie = text
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case("cookie: session=ok"));
                let has_auth = text
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case("authorization: Bearer secret"));
                if !has_cookie || !has_auth {
                    let _ = stream
                        .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                // Inject two transient 503 responses to test automatic backoff retries!
                let nreq = cnt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if nreq < 2 {
                    let _ = stream
                        .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let method = text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("GET");
                let range = text.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("range")
                        .then(|| v.trim().to_owned())
                });
                let total = data.len();
                if method == "HEAD" {
                    let h = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(h.as_bytes()).await;
                    return;
                }
                let (s, e) = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.split_once('-'))
                    .map(|(a, b)| {
                        let s = a.parse::<usize>().unwrap_or(0);
                        let e = b.parse::<usize>().unwrap_or(total - 1).min(total - 1);
                        (s, e)
                    })
                    .unwrap_or((0, total - 1));
                let slice = &data[s..=e];
                let hdr = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(hdr.as_bytes()).await;
                // Simulate a mid-stream TCP drop once on the first large slice!
                if slice.len() > 64 * 1024
                    && !dropped.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = stream.write_all(&slice[..32 * 1024]).await;
                    return; // abruptly close connection mid-body!
                }
                let _ = stream.write_all(slice).await;
            });
        }
    });

    // Extensionless signed URL + filename hint + Cookie/Auth headers + 2 Multi-NIC clients!
    let url = Url::parse(&format!("http://{addr}/dl/8f7e6d?sig=abc")).unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::COOKIE, "session=ok".parse().unwrap());
    headers.insert(
        reqwest::header::AUTHORIZATION,
        "Bearer secret".parse().unwrap(),
    );
    let mut http = blazingdl_extract::UnpackHttp::new(
        vec![
            (Some("Ethernet".into()), reqwest::Client::new()),
            (Some("Wi-Fi".into()), reqwest::Client::new()),
        ],
        headers,
    );
    http.retry_base = std::time::Duration::from_millis(20);
    http.parallel_store_threshold = 128 * 1024;

    let dest = tempfile::tempdir().unwrap();
    let prog = Arc::new(blazingdl_extract::UnpackProgress::default());
    let rep = blazingdl_extract::stream_unpack::unpack_url_with_http_and_hint(
        &http,
        &url,
        Some("pack.zip"),
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
        Some(prog.clone()),
    )
    .await
    .expect("must recover from 503 + mid-stream TCP drop with headers and Multi-NIC");

    assert_eq!(rep.files, 1);
    assert_eq!(
        std::fs::read(dest.path().join("signed/data.bin")).unwrap(),
        payload
    );
    let nic = prog.snapshot_nic_bytes();
    assert!(
        nic.iter().any(|(l, b)| l == "Ethernet" && *b > 0)
            && nic.iter().any(|(l, b)| l == "Wi-Fi" && *b > 0),
        "expected bytes recorded on both NIC adapters, got {nic:?}"
    );
    srv.abort();
}

#[tokio::test]
async fn non_range_200_ok_returns_unsupported_for_fallback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = vec![0u8; 200 * 1024];
    let body_srv = Arc::new(body);
    let srv = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let data = body_srv.clone();
            tokio::spawn(async move {
                let mut req_buf = vec![0u8; 2048];
                let Ok(n) = stream.read(&mut req_buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let text = String::from_utf8_lossy(&req_buf[..n]);
                let method = text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("GET");
                let h = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                );
                let _ = stream.write_all(h.as_bytes()).await;
                if method != "HEAD" {
                    let _ = stream.write_all(&data).await;
                }
            });
        }
    });

    let url = Url::parse(&format!("http://{addr}/no-range.zip")).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let http = blazingdl_extract::UnpackHttp::single(reqwest::Client::new());
    let err = blazingdl_extract::stream_unpack::unpack_url_with_http(
        &http,
        &url,
        dest.path(),
        4,
        Arc::new(AtomicBool::new(false)),
        None,
    )
    .await
    .expect_err("non-Range 200 OK must return Unsupported");
    assert!(err.is_unsupported(), "expected Unsupported, got {err}");
    srv.abort();
}
