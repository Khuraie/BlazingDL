//! Native Usenet (`.nzb`) XML parser, yEnc decoder with CRC32 verification,
//! and multi-connection async NNTP/NNTPS downloader.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NzbSegment {
    pub number: u32,
    pub bytes: u64,
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NzbFile {
    pub subject: String,
    pub filename: String,
    pub poster: String,
    pub groups: Vec<String>,
    pub segments: Vec<NzbSegment>,
}

impl NzbFile {
    pub fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nzb {
    pub files: Vec<NzbFile>,
}

impl Nzb {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(NzbFile::total_bytes).sum()
    }
}

/// NNTP server settings (stored in app Settings; TLS recommended on port 563).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NntpConfig {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
    pub password: String,
    pub connections: u32,
}

impl Default for NntpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 563,
            tls: true,
            username: String::new(),
            password: String::new(),
            connections: 8,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NzbError {
    #[error("invalid NZB XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("not an NZB file")]
    NotNzb,
    #[error("NZB contains no files or segments")]
    Empty,
    #[error("NNTP server host is not configured")]
    NoServer,
    #[error("yEnc decode error: {0}")]
    Yenc(String),
    #[error("yEnc CRC32 mismatch: expected {expected:08x}, got {actual:08x}")]
    CrcMismatch { expected: u32, actual: u32 },
    #[error("NNTP protocol error: {0}")]
    Protocol(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cancelled")]
    Cancelled,
}

/// True when `path` has `.nzb` extension.
pub fn is_nzb_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("nzb"))
}

/// Parse an `.nzb` XML document into files and ordered segments.
pub fn parse_nzb(xml: &str) -> Result<Nzb, NzbError> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "nzb" {
        return Err(NzbError::NotNzb);
    }
    let mut files = Vec::new();
    for fnode in root
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "file")
    {
        let subject = fnode.attribute("subject").unwrap_or("").trim().to_owned();
        let poster = fnode.attribute("poster").unwrap_or("").trim().to_owned();
        let mut groups = Vec::new();
        let mut segments = Vec::new();
        for child in fnode.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                "groups" => {
                    for g in child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "group")
                    {
                        if let Some(t) = g.text().map(str::trim).filter(|t| !t.is_empty()) {
                            groups.push(t.to_owned());
                        }
                    }
                }
                "segments" => {
                    for s in child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "segment")
                    {
                        let number = s
                            .attribute("number")
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(0);
                        let bytes = s
                            .attribute("bytes")
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0);
                        if let Some(id) = s.text().map(str::trim).filter(|t| !t.is_empty()) {
                            segments.push(NzbSegment {
                                number,
                                bytes,
                                message_id: id.to_owned(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        segments.sort_by_key(|s| s.number);
        if !segments.is_empty() {
            let filename = filename_from_subject(&subject);
            files.push(NzbFile {
                subject,
                filename,
                poster,
                groups,
                segments,
            });
        }
    }
    if files.is_empty() {
        return Err(NzbError::Empty);
    }
    Ok(Nzb { files })
}

/// Extract clean filename from Usenet subject convention, e.g. `"[1/3] \"archive.part1.rar\" yEnc (1/40)"`.
pub fn filename_from_subject(subject: &str) -> String {
    if let Some(start) = subject.find('"') {
        let rest = &subject[start + 1..];
        if let Some(end) = rest.find('"') {
            let candidate = rest[..end].trim();
            if !candidate.is_empty() {
                return crate::naming::sanitize(candidate);
            }
        }
    }
    let clean = subject.split(" yEnc").next().unwrap_or(subject).trim();
    let fallback = if clean.is_empty() {
        "usenet.bin"
    } else {
        clean
    };
    crate::naming::sanitize(fallback)
}

/// One decoded yEnc article payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YencPart {
    pub name: String,
    pub total_size: u64,
    /// 0-based byte offset in destination file.
    pub offset: u64,
    pub data: Vec<u8>,
    pub crc32: Option<u32>,
}

/// Decode a yEnc-encoded NNTP article body (`=ybegin` / optional `=ypart` / `=yend`) and verify CRC32.
pub fn decode_yenc(raw: &[u8]) -> Result<YencPart, NzbError> {
    let mut name = String::new();
    let mut total_size = 0u64;
    let mut offset = 0u64;
    let mut expected_crc: Option<u32> = None;
    let mut in_body = false;
    let mut out = Vec::with_capacity(raw.len());

    for line in raw.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !in_body {
            if let Some(rest) = line.strip_prefix(b"=ybegin ") {
                let s = String::from_utf8_lossy(rest);
                if let Some(sz) = kv_u64(&s, "size=") {
                    total_size = sz;
                }
                if let Some(n) = s.split("name=").nth(1) {
                    name = n.trim().to_owned();
                }
                in_body = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"=ypart ") {
            let s = String::from_utf8_lossy(rest);
            if let Some(begin) = kv_u64(&s, "begin=") {
                offset = begin.saturating_sub(1);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"=yend ") {
            let s = String::from_utf8_lossy(rest);
            expected_crc = kv_hex_u32(&s, "pcrc32=").or_else(|| kv_hex_u32(&s, "crc32="));
            break;
        }
        // NNTP dot-unstuffing (leading ".." -> ".").
        let line = line.strip_prefix(b"..").map_or(line, |r| {
            // Re-emit one leading dot decoded normally below.
            out.push(b'.'.wrapping_sub(42));
            r
        });
        let mut i = 0;
        while i < line.len() {
            let b = line[i];
            if b == b'=' && i + 1 < line.len() {
                i += 1;
                out.push(line[i].wrapping_sub(64).wrapping_sub(42));
            } else {
                out.push(b.wrapping_sub(42));
            }
            i += 1;
        }
    }
    if !in_body {
        return Err(NzbError::Yenc("missing =ybegin header".into()));
    }
    if let Some(expected) = expected_crc {
        let actual = crc32fast::hash(&out);
        if actual != expected {
            return Err(NzbError::CrcMismatch { expected, actual });
        }
    }
    Ok(YencPart {
        name,
        total_size,
        offset,
        data: out,
        crc32: expected_crc,
    })
}

/// Encode bytes into a valid yEnc part (used in tests and tooling).
pub fn encode_yenc(name: &str, total_size: u64, offset: u64, data: &[u8]) -> Vec<u8> {
    let crc = crc32fast::hash(data);
    let mut out = Vec::with_capacity(data.len() + 128);
    let _ = writeln!(
        out,
        "=ybegin part=1 line=128 size={total_size} name={name}\r"
    );
    let _ = writeln!(
        out,
        "=ypart begin={} end={}\r",
        offset + 1,
        offset + data.len() as u64
    );
    let mut col = 0usize;
    for &b in data {
        let enc = b.wrapping_add(42);
        let escape = matches!(enc, 0x00 | 0x0A | 0x0D | b'=') || (col == 0 && enc == b'.');
        if escape {
            out.push(b'=');
            out.push(enc.wrapping_add(64));
            col += 2;
        } else {
            out.push(enc);
            col += 1;
        }
        if col >= 128 {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }
    if col > 0 {
        out.extend_from_slice(b"\r\n");
    }
    let _ = writeln!(out, "=yend size={} part=1 pcrc32={crc:08x}\r", data.len());
    out
}

fn kv_u64(s: &str, key: &str) -> Option<u64> {
    let rest = s.split(key).nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

fn kv_hex_u32(s: &str, key: &str) -> Option<u32> {
    let rest = s.split(key).nth(1)?;
    u32::from_str_radix(rest.split_whitespace().next()?, 16).ok()
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

async fn connect_nntp(cfg: &NntpConfig) -> Result<BufReader<Box<dyn AsyncStream>>, NzbError> {
    if cfg.host.trim().is_empty() {
        return Err(NzbError::NoServer);
    }
    let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port)).await?;
    let stream: Box<dyn AsyncStream> = if cfg.tls {
        let mut roots = rustls::RootCertStore::empty();
        for cert in crate::tls::mozilla_roots() {
            let _ = roots.add(cert);
        }
        let tls_cfg = rustls::ClientConfig::builder_with_provider(crate::tls::provider())
            .with_safe_default_protocol_versions()
            .expect("default TLS protocol versions supported")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_cfg));
        let server_name = rustls::pki_types::ServerName::try_from(cfg.host.clone())
            .map_err(|_| NzbError::Protocol(format!("invalid TLS host: {}", cfg.host)))?;
        Box::new(connector.connect(server_name, tcp).await?)
    } else {
        Box::new(tcp)
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if !line.starts_with("200") && !line.starts_with("201") {
        return Err(NzbError::Protocol(format!(
            "unexpected greeting: {}",
            line.trim()
        )));
    }
    if !cfg.username.is_empty() {
        reader
            .get_mut()
            .write_all(format!("AUTHINFO USER {}\r\n", cfg.username).as_bytes())
            .await?;
        line.clear();
        reader.read_line(&mut line).await?;
        if line.starts_with("381") {
            reader
                .get_mut()
                .write_all(format!("AUTHINFO PASS {}\r\n", cfg.password).as_bytes())
                .await?;
            line.clear();
            reader.read_line(&mut line).await?;
        }
        if !line.starts_with("281") {
            return Err(NzbError::Protocol(format!(
                "auth rejected: {}",
                line.trim()
            )));
        }
    }
    Ok(reader)
}

async fn fetch_body(
    conn: &mut BufReader<Box<dyn AsyncStream>>,
    message_id: &str,
) -> Result<Vec<u8>, NzbError> {
    let cmd = format!("BODY <{}>\r\n", message_id.trim_matches(['<', '>']));
    conn.get_mut().write_all(cmd.as_bytes()).await?;
    let mut status = String::new();
    conn.read_line(&mut status).await?;
    if !status.starts_with("222") {
        return Err(NzbError::Protocol(format!(
            "BODY <{message_id}> failed: {}",
            status.trim()
        )));
    }
    let mut body = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = conn.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            return Err(NzbError::Protocol("unexpected EOF reading article".into()));
        }
        if buf == b".\r\n" || buf == b".\n" {
            break;
        }
        body.extend_from_slice(&buf);
    }
    Ok(body)
}

/// Download every file in `nzb` into `dest_dir` over parallel NNTP connections, decoding yEnc
/// parts and verifying CRC32 per segment.
/// Convenience helper to download an NZB without manually creating progress/cancellation handles.
pub async fn download_nzb_simple(
    nzb: &Nzb,
    cfg: &NntpConfig,
    dest_dir: &Path,
) -> Result<Vec<PathBuf>, NzbError> {
    download_nzb(
        nzb,
        cfg,
        dest_dir,
        Arc::new(AtomicU64::new(0)),
        CancellationToken::new(),
    )
    .await
}

pub async fn download_nzb(
    nzb: &Nzb,
    cfg: &NntpConfig,
    dest_dir: &Path,
    downloaded: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> Result<Vec<PathBuf>, NzbError> {
    std::fs::create_dir_all(dest_dir)?;
    let mut saved_files = Vec::new();
    let workers = cfg.connections.clamp(1, 32) as usize;

    for nzb_file in &nzb.files {
        if cancel.is_cancelled() {
            return Err(NzbError::Cancelled);
        }
        let target = dest_dir.join(&nzb_file.filename);
        let file = Arc::new(std::sync::Mutex::new(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&target)?,
        ));
        let queue = Arc::new(tokio::sync::Mutex::new(
            nzb_file
                .segments
                .clone()
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        ));
        let mut handles = Vec::new();
        let n_workers = workers.min(nzb_file.segments.len().max(1));
        for _ in 0..n_workers {
            let cfg = cfg.clone();
            let queue = queue.clone();
            let file = file.clone();
            let downloaded = downloaded.clone();
            let cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                let mut conn = connect_nntp(&cfg).await?;
                loop {
                    if cancel.is_cancelled() {
                        return Err(NzbError::Cancelled);
                    }
                    let seg = { queue.lock().await.pop_front() };
                    let Some(seg) = seg else { break };
                    let raw = fetch_body(&mut conn, &seg.message_id).await?;
                    let part = decode_yenc(&raw)?;
                    let len = part.data.len() as u64;
                    let file_clone = file.clone();
                    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                        let mut f = file_clone.lock().unwrap();
                        f.seek(SeekFrom::Start(part.offset))?;
                        f.write_all(&part.data)?;
                        Ok(())
                    })
                    .await
                    .map_err(|e| NzbError::Protocol(e.to_string()))??;
                    downloaded.fetch_add(len, Ordering::Relaxed);
                }
                let _ = conn.get_mut().write_all(b"QUIT\r\n").await;
                Ok::<(), NzbError>(())
            }));
        }
        for h in handles {
            h.await.map_err(|e| NzbError::Protocol(e.to_string()))??;
        }
        saved_files.push(target);
    }
    Ok(saved_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_nzb_and_extracts_filename() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
        <nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
          <file poster="bot@usenet" date="1700000000" subject='[1/2] "release.bin" yEnc (1/2)'>
            <groups><group>alt.binaries.test</group></groups>
            <segments>
              <segment bytes="512" number="2">part2@example.invalid</segment>
              <segment bytes="512" number="1">part1@example.invalid</segment>
            </segments>
          </file>
        </nzb>"#;
        let nzb = parse_nzb(xml).unwrap();
        assert_eq!(nzb.files.len(), 1);
        assert_eq!(nzb.files[0].filename, "release.bin");
        assert_eq!(nzb.files[0].segments[0].number, 1);
        assert_eq!(nzb.files[0].segments[1].number, 2);
        assert_eq!(nzb.total_bytes(), 1024);
    }

    #[test]
    fn yenc_round_trip_and_crc_check() {
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let encoded = encode_yenc("sample.bin", 4096, 0, &payload);
        let decoded = decode_yenc(&encoded).unwrap();
        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.offset, 0);
        // Corrupt one byte -> CRC32 mismatch caught.
        let mut bad = encoded.clone();
        let mid = bad.len() / 2;
        bad[mid] = bad[mid].wrapping_add(1);
        assert!(matches!(
            decode_yenc(&bad),
            Err(NzbError::CrcMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn downloads_multipart_nzb_from_local_nntp_server() {
        let part1 = b"Hello from BlazingDL Usenet part 1! ".repeat(40);
        let part2 = b"And part 2 reassembled via yEnc CRC32!".repeat(40);
        let total = (part1.len() + part2.len()) as u64;
        let enc1 = Arc::new(encode_yenc("hello.txt", total, 0, &part1));
        let enc2 = Arc::new(encode_yenc("hello.txt", total, part1.len() as u64, &part2));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn({
            let (enc1, enc2) = (enc1.clone(), enc2.clone());
            async move {
                for _ in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (enc1, enc2) = (enc1.clone(), enc2.clone());
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stream);
                        let _ = reader.get_mut().write_all(b"200 Mock NNTP Ready\r\n").await;
                        let mut line = String::new();
                        loop {
                            line.clear();
                            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                break;
                            }
                            if line.starts_with("BODY <p1@test>") {
                                let _ = reader.get_mut().write_all(b"222 body follows\r\n").await;
                                let _ = reader.get_mut().write_all(&enc1).await;
                                let _ = reader.get_mut().write_all(b".\r\n").await;
                            } else if line.starts_with("BODY <p2@test>") {
                                let _ = reader.get_mut().write_all(b"222 body follows\r\n").await;
                                let _ = reader.get_mut().write_all(&enc2).await;
                                let _ = reader.get_mut().write_all(b".\r\n").await;
                            } else if line.starts_with("QUIT") {
                                let _ = reader.get_mut().write_all(b"205 bye\r\n").await;
                                break;
                            }
                        }
                    });
                }
            }
        });

        let xml = format!(
            r#"<nzb><file subject='"hello.txt" yEnc (1/2)'><segments>
                <segment bytes="{}" number="1">p1@test</segment>
                <segment bytes="{}" number="2">p2@test</segment>
            </segments></file></nzb>"#,
            part1.len(),
            part2.len()
        );
        let nzb = parse_nzb(&xml).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = NntpConfig {
            host: "127.0.0.1".into(),
            port,
            tls: false,
            username: String::new(),
            password: String::new(),
            connections: 2,
        };
        let progress = Arc::new(AtomicU64::new(0));
        let out = download_nzb(
            &nzb,
            &cfg,
            dir.path(),
            progress.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 1);
        let mut expected = part1;
        expected.extend_from_slice(&part2);
        assert_eq!(std::fs::read(&out[0]).unwrap(), expected);
        assert_eq!(progress.load(Ordering::Relaxed), total);
        server.abort();
    }
}
