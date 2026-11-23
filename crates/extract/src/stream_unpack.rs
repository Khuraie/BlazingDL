//! Steam-style 1× disk space streaming download + decompression (inspired by and wire-compatible
//! with `Khuraie/AntiCompress`).
//!
//! Supported streaming targets (the archive never sits on disk alongside its unpacked files):
//! - **Remote `.zip` (incl. ZIP64 >300 GiB)** via HTTP `Range` Central Directory + true chunked
//!   streaming entry decompression (Store, Deflate, Zstd; constant <512 KiB RAM per worker) +
//!   CRC32 & exact-length verification + atomic `.anticompress-resume.json` + mid-entry byte resume
//!   for STORE entries.
//! - **Remote `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.tar.zst`/`.tzst`** piped asynchronously
//!   straight to disk with instant cancellation, atomic `.acpart` renames, `MultiGzDecoder` for
//!   multi-member gzip, and HTTP `Range` entry-boundary resume for plain `.tar`.
//! - **AntiCompress `.acpkg` v1** (local folder or remote HTTP/HTTPS): canonical SHA-256 manifest
//!   verification, parallel `chunk-NNNNNN.zst` fetch + per-chunk SHA-256 + bounded zstd decode,
//!   per-file SHA-256 verification, atomic `.acpart` renames, and accurate resumed progress.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::safe;

/// AntiCompress `.acpkg` wire format constants (exact match with `Khuraie/AntiCompress`).
pub const ACPKG_VERSION: u32 = 1;
pub const ACPKG_CHUNK_SIZE: u64 = 1024 * 1024; // 1 MiB decompressed per chunk
pub const ACPKG_MANIFEST_NAME: &str = "manifest.json";
pub const RESUME_FILE_NAME: &str = ".anticompress-resume.json";

/// Maximum ZIP Central Directory fetched into memory (256 MiB — enough for ~2.5 million entries).
const MAX_CD_BYTES: u64 = 256 * 1024 * 1024;
/// Streaming I/O buffer size (128 KiB — fast on NVMe/HDD while keeping RAM bounded on 300+ GB files).
const IO_BUF_SIZE: usize = 128 * 1024;
/// Maximum live events kept in [`UnpackProgress`].
const MAX_PROGRESS_EVENTS: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamUnpackReport {
    pub files: u64,
    pub bytes_written: u64,
    pub bytes_downloaded: u64,
    pub dest_dir: PathBuf,
    pub skipped_resumed: u64,
}

/// Live progress counters, speed limiter, and event log shared between a streaming unpack task and
/// the UI transfer row / progress window.
#[derive(Default, Debug)]
pub struct UnpackProgress {
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
    pub active_workers: AtomicU64,
    /// Optional speed limit in bytes/sec (0 = unlimited), live-updatable from Speed Limiter / Focus Mode.
    pub speed_limit: AtomicU64,
    events: Mutex<Vec<(u64, SystemTime, String)>>,
    next_event_seq: AtomicU64,
    nic_bytes: Mutex<BTreeMap<String, u64>>,
}

impl UnpackProgress {
    pub fn push_event(&self, text: impl Into<String>) {
        let seq = self.next_event_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut guard) = self.events.lock() {
            if guard.len() >= MAX_PROGRESS_EVENTS {
                guard.remove(0);
            }
            guard.push((seq, SystemTime::now(), text.into()));
        }
    }

    pub fn snapshot_events(&self) -> Vec<(u64, SystemTime, String)> {
        self.events.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn record_nic_bytes(&self, label: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Ok(mut guard) = self.nic_bytes.lock() {
            *guard.entry(label.to_owned()).or_insert(0) += bytes;
        }
    }

    pub fn snapshot_nic_bytes(&self) -> Vec<(String, u64)> {
        self.nic_bytes
            .lock()
            .map(|g| g.iter().map(|(k, &v)| (k.clone(), v)).collect())
            .unwrap_or_default()
    }

    pub fn set_speed_limit(&self, rate: Option<u64>) {
        self.speed_limit.store(rate.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn speed_limit(&self) -> Option<u64> {
        let v = self.speed_limit.load(Ordering::Relaxed);
        (v > 0).then_some(v)
    }
}

/// Engine-grade HTTP transport configuration for streaming unpack: supports Multi-NIC round-robin
/// clients, custom headers (Cookie, Referer, Authorization, User-Agent), automatic exponential
/// backoff retries on transient drops, and parallel multi-connection slicing for large ZIP STORE
/// entries.
#[derive(Clone)]
pub struct UnpackHttp {
    /// One or more configured `reqwest::Client`s paired with optional adapter labels.
    pub clients: Vec<(Option<String>, reqwest::Client)>,
    /// Extra HTTP headers attached to every request (Cookie, Referer, Authorization, etc.).
    pub headers: reqwest::header::HeaderMap,
    /// Maximum retry attempts on transient transport / HTTP 408/429/5xx errors (default: 5).
    pub max_retries: usize,
    /// Base backoff delay before retrying (exponential, capped at 10 s; default: 400 ms).
    pub retry_base: Duration,
    /// Minimum uncompressed size for a ZIP STORE entry to be split across parallel Range
    /// connections when `concurrency > 1` (default: 16 MiB).
    pub parallel_store_threshold: u64,
    rr: Arc<AtomicU64>,
}

impl UnpackHttp {
    pub fn single(client: reqwest::Client) -> Self {
        Self::new(vec![(None, client)], reqwest::header::HeaderMap::new())
    }

    pub fn new(
        mut clients: Vec<(Option<String>, reqwest::Client)>,
        headers: reqwest::header::HeaderMap,
    ) -> Self {
        if clients.is_empty() {
            clients.push((None, reqwest::Client::new()));
        }
        Self {
            clients,
            headers,
            max_retries: 5,
            retry_base: Duration::from_millis(400),
            parallel_store_threshold: 16 * 1024 * 1024,
            rr: Arc::new(AtomicU64::new(0)),
        }
    }

    fn is_multi_nic(&self) -> bool {
        self.clients.len() > 1
    }

    fn pick(&self) -> (Option<String>, reqwest::Client) {
        let idx = (self.rr.fetch_add(1, Ordering::Relaxed) as usize) % self.clients.len();
        self.clients[idx].clone()
    }

    fn get(&self, url: &Url) -> (Option<String>, reqwest::RequestBuilder) {
        let (label, client) = self.pick();
        let mut req = client.get(url.clone());
        if !self.headers.is_empty() {
            req = req.headers(self.headers.clone());
        }
        (label, req)
    }

    fn head(&self, url: &Url) -> (Option<String>, reqwest::RequestBuilder) {
        let (label, client) = self.pick();
        let mut req = client.head(url.clone());
        if !self.headers.is_empty() {
            req = req.headers(self.headers.clone());
        }
        (label, req)
    }

    fn record_nic(&self, progress: Option<&Arc<UnpackProgress>>, label: Option<&str>, bytes: u64) {
        if self.is_multi_nic()
            && let (Some(p), Some(lbl)) = (progress, label)
        {
            p.record_nic_bytes(lbl, bytes);
        }
    }

    async fn backoff(&self, attempt: usize, cancel: &AtomicBool) -> Result<(), StreamUnpackError> {
        let shift = attempt.min(5) as u32;
        let delay = self
            .retry_base
            .saturating_mul(1u32 << shift)
            .min(Duration::from_secs(10));
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if cancel.load(Ordering::Relaxed) {
                return Err(StreamUnpackError::Cancelled);
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(rem.min(Duration::from_millis(50))).await;
        }
        Ok(())
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

/// Cooperative async token-bucket pacing against [`UnpackProgress::speed_limit`].
async fn pace_async(progress: Option<&Arc<UnpackProgress>>, bytes: u64, cancel: &AtomicBool) {
    let Some(p) = progress else { return };
    let limit = p.speed_limit.load(Ordering::Relaxed);
    if limit == 0 || bytes == 0 {
        return;
    }
    let workers = p.active_workers.load(Ordering::Relaxed).max(1);
    let per_worker = (limit / workers).max(1024);
    let wait_secs = (bytes as f64 / per_worker as f64).min(2.0);
    if wait_secs < 0.002 {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(wait_secs);
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let rem = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(rem.min(Duration::from_millis(50))).await;
    }
}

#[derive(Debug)]
pub enum StreamUnpackError {
    Io(io::Error),
    Http(String),
    BadArchive(String),
    Integrity(String),
    Unsupported(String),
    Cancelled,
}

impl StreamUnpackError {
    /// True when the remote server or archive cannot be stream-unpacked (e.g. server ignored
    /// HTTP Range, encrypted ZIP, or unsupported method), signalling the caller to fall back
    /// cleanly to a standard download + post-extract.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported(_))
    }
}

impl std::fmt::Display for StreamUnpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Http(s) => write!(f, "HTTP error: {s}"),
            Self::BadArchive(s) => write!(f, "invalid archive: {s}"),
            Self::Integrity(s) => write!(f, "integrity check failed: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported stream format: {s}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for StreamUnpackError {}

impl From<io::Error> for StreamUnpackError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Atomically write JSON state via sibling temp file + rename so crashes/pauses never corrupt resume state.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
    }
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
}

/// True when `path` is a local `.acpkg` directory (containing `manifest.json`) or an `.acpkg`
/// `manifest.json` file.
pub fn is_acpkg_path(path: &Path) -> bool {
    if path.is_dir()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("acpkg"))
        && path.join(ACPKG_MANIFEST_NAME).is_file()
    {
        return true;
    }
    path.is_file()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(ACPKG_MANIFEST_NAME))
        && path
            .parent()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("acpkg"))
}

/// True when a filename (e.g. from `Content-Disposition` or the browser) has a stream-unpackable
/// archive extension (`.zip`, `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.tar.zst`/`.tzst`, `.acpkg`).
pub fn is_stream_unpack_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    trimmed.ends_with(".zip")
        || trimmed.ends_with(".tar")
        || trimmed.ends_with(".tar.gz")
        || trimmed.ends_with(".tgz")
        || trimmed.ends_with(".tar.xz")
        || trimmed.ends_with(".txz")
        || trimmed.ends_with(".tar.zst")
        || trimmed.ends_with(".tzst")
        || trimmed.ends_with(".acpkg")
}

/// True when `url` points to a stream-unpackable archive (`.zip`, `.tar.*`, or `.acpkg`).
pub fn is_stream_unpack_url(url: &Url) -> bool {
    let lower = url.path().to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    is_stream_unpack_name(trimmed) || trimmed.ends_with("/manifest.json")
}

/// Clean folder name derived from a stream-unpackable URL or local `.acpkg` path.
pub fn folder_name_for_url(url: &Url) -> String {
    let path = url.path().trim_end_matches('/');
    let seg = if path.to_ascii_lowercase().ends_with("/manifest.json") {
        path.trim_end_matches("/manifest.json")
            .rsplit('/')
            .next()
            .unwrap_or("unpacked")
    } else {
        path.rsplit('/').next().unwrap_or("unpacked")
    };
    strip_archive_suffix(seg)
}

pub fn strip_archive_suffix(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".tar.gz", ".tar.xz", ".tar.zst", ".tgz", ".txz", ".tzst", ".zip", ".tar", ".acpkg",
    ] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return blazingdl_engine::naming::sanitize(&name[..name.len() - ext.len()]);
        }
    }
    let clean = blazingdl_engine::naming::sanitize(name);
    if clean.is_empty() {
        "unpacked".to_owned()
    } else {
        clean
    }
}

// ============================================================================
// 1. AntiCompress `.acpkg` v1 (exact wire compatibility with Khuraie/AntiCompress)
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcChunkInfo {
    pub index: usize,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcFileEntry {
    pub path: String,
    pub size: u64,
    pub offset: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcManifest {
    pub version: u32,
    pub chunk_size: u64,
    pub total_size: u64,
    pub files: Vec<AcFileEntry>,
    pub chunks: Vec<AcChunkInfo>,
    pub manifest_sha256: String,
}

impl AcManifest {
    pub fn chunk_name(index: usize) -> String {
        format!("chunk-{index:06}.zst")
    }

    pub fn chunk_expected_size(&self, index: usize) -> u64 {
        if index + 1 < self.chunks.len() {
            self.chunk_size
        } else {
            self.total_size
                .saturating_sub((index as u64).saturating_mul(self.chunk_size))
        }
    }
}

/// Produce canonical sorted-key compact JSON identical to Python's
/// `json.dumps(d, sort_keys=True, separators=(",", ":"))` for SHA-256 self-hash verification.
fn canonical_manifest_json(m: &AcManifest, hash_value: &str) -> String {
    let mut root: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    let chunks_val: Vec<serde_json::Value> = m
        .chunks
        .iter()
        .map(|c| {
            let mut o = serde_json::Map::new();
            o.insert("index".into(), serde_json::json!(c.index));
            o.insert("sha256".into(), serde_json::json!(c.sha256));
            serde_json::Value::Object(o)
        })
        .collect();
    let files_val: Vec<serde_json::Value> = m
        .files
        .iter()
        .map(|f| {
            let mut o = serde_json::Map::new();
            o.insert("offset".into(), serde_json::json!(f.offset));
            o.insert("path".into(), serde_json::json!(f.path));
            o.insert("sha256".into(), serde_json::json!(f.sha256));
            o.insert("size".into(), serde_json::json!(f.size));
            serde_json::Value::Object(o)
        })
        .collect();
    root.insert("chunk_size", serde_json::json!(m.chunk_size));
    root.insert("chunks", serde_json::Value::Array(chunks_val));
    root.insert("files", serde_json::Value::Array(files_val));
    root.insert("manifest_sha256", serde_json::json!(hash_value));
    root.insert("total_size", serde_json::json!(m.total_size));
    root.insert("version", serde_json::json!(m.version));
    serde_json::to_string(&root).expect("canonical JSON serialization")
}

/// Serialize an [`AcManifest`] with its `manifest_sha256` self-hash populated.
pub fn serialize_acpkg_manifest(m: &AcManifest) -> String {
    let body = canonical_manifest_json(m, "");
    let hash = hex::encode(Sha256::digest(body.as_bytes()));
    canonical_manifest_json(m, &hash)
}

/// Parse and cryptographically verify an AntiCompress v1 `manifest.json`.
pub fn parse_acpkg_manifest(text: &str) -> Result<AcManifest, StreamUnpackError> {
    let m: AcManifest = serde_json::from_str(text)
        .map_err(|e| StreamUnpackError::BadArchive(format!("manifest.json parse error: {e}")))?;
    if m.version != ACPKG_VERSION {
        return Err(StreamUnpackError::Unsupported(format!(
            "unsupported .acpkg version {}",
            m.version
        )));
    }
    if m.chunk_size != ACPKG_CHUNK_SIZE {
        return Err(StreamUnpackError::Unsupported(format!(
            "unsupported .acpkg chunk size {}",
            m.chunk_size
        )));
    }
    let body = canonical_manifest_json(&m, "");
    let actual = hex::encode(Sha256::digest(body.as_bytes()));
    if !actual.eq_ignore_ascii_case(&m.manifest_sha256) {
        return Err(StreamUnpackError::Integrity(
            "manifest_sha256 mismatch (tampered or corrupt manifest)".into(),
        ));
    }
    Ok(m)
}

fn verify_existing_file(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<bool, StreamUnpackError> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(false);
    };
    if !meta.is_file() || meta.len() != expected_size {
        return Ok(false);
    }
    let Ok(mut f) = File::open(path) else {
        return Ok(false);
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; IO_BUF_SIZE];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return Ok(false),
        }
    }
    Ok(hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_sha256))
}

fn decompress_zstd_chunk(compressed: &[u8], max_len: u64) -> Result<Vec<u8>, StreamUnpackError> {
    let dec = ruzstd::StreamingDecoder::new(compressed)
        .map_err(|e| StreamUnpackError::BadArchive(format!("zstd header error: {e}")))?;
    let cap = max_len.saturating_add(1);
    let mut out = Vec::with_capacity(max_len.min(ACPKG_CHUNK_SIZE) as usize);
    dec.take(cap)
        .read_to_end(&mut out)
        .map_err(|e| StreamUnpackError::BadArchive(format!("zstd decode error: {e}")))?;
    if out.len() as u64 > max_len {
        return Err(StreamUnpackError::Integrity(format!(
            "zstd chunk exceeded expected size ({max_len} bytes)"
        )));
    }
    Ok(out)
}

/// Assembles files from sequential decompressed chunks, verifying per-file SHA-256 and writing
/// atomically via `.acpart` temp files.
struct AcAssembler {
    dest_dir: PathBuf,
    files: Vec<AcFileEntry>,
    skip_mask: Vec<bool>,
    file_idx: usize,
    file_written: u64,
    stream_pos: u64,
    active_tmp: Option<(PathBuf, PathBuf, BufWriter<File>, Sha256)>,
    files_done: u64,
    bytes_written: u64,
    already_bytes: u64,
    skipped_resumed: u64,
}

impl Drop for AcAssembler {
    fn drop(&mut self) {
        if let Some((tmp, _, mut w, _)) = self.active_tmp.take() {
            let _ = w.flush();
            drop(w);
            let _ = fs::remove_file(&tmp);
        }
    }
}

impl AcAssembler {
    fn new(
        dest_dir: &Path,
        files: Vec<AcFileEntry>,
        cancel: &AtomicBool,
    ) -> Result<Self, StreamUnpackError> {
        fs::create_dir_all(dest_dir)?;
        let mut skip_mask = vec![false; files.len()];
        let mut skipped_resumed = 0u64;
        let mut already_bytes = 0u64;
        for (i, entry) in files.iter().enumerate() {
            let rel = safe::rel_path(&entry.path, 64).map_err(|why| {
                StreamUnpackError::BadArchive(format!("unsafe path {}: {why}", entry.path))
            })?;
            let final_path = dest_dir.join(&rel);
            if verify_existing_file(&final_path, entry.size, &entry.sha256, cancel)? {
                skip_mask[i] = true;
                skipped_resumed += 1;
                already_bytes = already_bytes.saturating_add(entry.size);
            }
        }
        Ok(Self {
            dest_dir: dest_dir.to_owned(),
            files,
            skip_mask,
            file_idx: 0,
            file_written: 0,
            stream_pos: 0,
            active_tmp: None,
            files_done: 0,
            bytes_written: 0,
            already_bytes,
            skipped_resumed,
        })
    }

    fn needed_chunk_indices(
        &self,
        chunk_size: u64,
        num_chunks: usize,
    ) -> Result<HashSet<usize>, StreamUnpackError> {
        if chunk_size == 0 {
            return Err(StreamUnpackError::BadArchive("zero chunk_size".into()));
        }
        let mut needed = HashSet::new();
        for (i, f) in self.files.iter().enumerate() {
            if self.skip_mask[i] || f.size == 0 {
                continue;
            }
            let end_byte = f
                .offset
                .checked_add(f.size - 1)
                .ok_or_else(|| StreamUnpackError::BadArchive("file offset overflow".into()))?;
            let first = usize::try_from(f.offset / chunk_size)
                .map_err(|_| StreamUnpackError::BadArchive("chunk index overflow".into()))?;
            let last = usize::try_from(end_byte / chunk_size)
                .map_err(|_| StreamUnpackError::BadArchive("chunk index overflow".into()))?;
            for c in first..=last.min(num_chunks.saturating_sub(1)) {
                needed.insert(c);
            }
        }
        Ok(needed)
    }

    fn finish_zero_byte_files(&mut self) -> Result<(), StreamUnpackError> {
        while self.file_idx < self.files.len() {
            let entry = &self.files[self.file_idx];
            if entry.size > 0 || entry.offset > self.stream_pos {
                break;
            }
            if !self.skip_mask[self.file_idx] {
                let rel = safe::rel_path(&entry.path, 64)
                    .map_err(|w| StreamUnpackError::BadArchive(w.into()))?;
                let dst = self.dest_dir.join(rel);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                let empty_sha = hex::encode(Sha256::digest(b""));
                if !empty_sha.eq_ignore_ascii_case(&entry.sha256) {
                    return Err(StreamUnpackError::Integrity(format!(
                        "SHA-256 mismatch on empty file {}",
                        entry.path
                    )));
                }
                File::create(&dst)?;
                self.files_done += 1;
            }
            self.file_idx += 1;
            self.file_written = 0;
        }
        Ok(())
    }

    fn feed_chunk(&mut self, chunk_offset: u64, data: &[u8]) -> Result<(), StreamUnpackError> {
        self.stream_pos = chunk_offset;
        self.finish_zero_byte_files()?;
        let chunk_end = chunk_offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| StreamUnpackError::BadArchive("chunk offset overflow".into()))?;

        while self.file_idx < self.files.len() {
            let entry = self.files[self.file_idx].clone();
            let file_end = entry
                .offset
                .checked_add(entry.size)
                .ok_or_else(|| StreamUnpackError::BadArchive("file end overflow".into()))?;
            if file_end <= chunk_offset {
                self.file_idx += 1;
                self.file_written = 0;
                continue;
            }
            if entry.offset >= chunk_end {
                break;
            }
            if self.skip_mask[self.file_idx] {
                if file_end <= chunk_end {
                    self.file_idx += 1;
                    self.file_written = 0;
                } else {
                    break;
                }
                continue;
            }

            if self.active_tmp.is_none() {
                let rel = safe::rel_path(&entry.path, 64)
                    .map_err(|w| StreamUnpackError::BadArchive(w.into()))?;
                let dst = self.dest_dir.join(rel);
                if let Some(p) = dst.parent() {
                    fs::create_dir_all(p)?;
                }
                let tmp = PathBuf::from(format!("{}.acpart", dst.display()));
                let f = BufWriter::with_capacity(IO_BUF_SIZE, File::create(&tmp)?);
                self.active_tmp = Some((tmp, dst, f, Sha256::new()));
                self.file_written = 0;
            }

            let cur_pos = entry.offset + self.file_written;
            let take_start = cur_pos.max(chunk_offset);
            let take_end = file_end.min(chunk_end);
            if take_end > take_start {
                let slice =
                    &data[(take_start - chunk_offset) as usize..(take_end - chunk_offset) as usize];
                if let Some((_, _, f, hasher)) = self.active_tmp.as_mut() {
                    f.write_all(slice)?;
                    hasher.update(slice);
                }
                self.file_written += slice.len() as u64;
                self.bytes_written += slice.len() as u64;
            }

            if self.file_written >= entry.size {
                if let Some((tmp, dst, mut f, hasher)) = self.active_tmp.take() {
                    f.flush()?;
                    drop(f);
                    let got = hex::encode(hasher.finalize());
                    if !got.eq_ignore_ascii_case(&entry.sha256) {
                        let _ = fs::remove_file(&tmp);
                        return Err(StreamUnpackError::Integrity(format!(
                            "SHA-256 mismatch for {}",
                            entry.path
                        )));
                    }
                    if dst.exists() {
                        let _ = fs::remove_file(&dst);
                    }
                    fs::rename(&tmp, &dst)?;
                    self.files_done += 1;
                }
                self.file_idx += 1;
                self.file_written = 0;
                self.stream_pos = take_end;
                self.finish_zero_byte_files()?;
            } else {
                break;
            }
        }
        self.stream_pos = chunk_end;
        self.finish_zero_byte_files()?;
        Ok(())
    }

    fn report(self, bytes_downloaded: u64) -> StreamUnpackReport {
        StreamUnpackReport {
            files: self.files_done,
            bytes_written: self.bytes_written,
            bytes_downloaded,
            dest_dir: self.dest_dir.clone(),
            skipped_resumed: self.skipped_resumed,
        }
    }
}

/// Install a local `.acpkg` directory (optionally deleting each chunk as it is consumed for 1× disk).
pub fn unpack_acpkg_local(
    pkg_path: &Path,
    dest_dir: &Path,
    delete_chunks: bool,
    cancel: &AtomicBool,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    unpack_acpkg_local_with_progress(pkg_path, dest_dir, delete_chunks, cancel, None)
}

/// [`unpack_acpkg_local`] with optional live progress counters for UI display.
pub fn unpack_acpkg_local_with_progress(
    pkg_path: &Path,
    dest_dir: &Path,
    delete_chunks: bool,
    cancel: &AtomicBool,
    progress: Option<&UnpackProgress>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let pkg_dir = if pkg_path.is_file() {
        pkg_path
            .parent()
            .ok_or_else(|| StreamUnpackError::BadArchive("invalid manifest path".into()))?
    } else {
        pkg_path
    };
    let manifest_text = fs::read_to_string(pkg_dir.join(ACPKG_MANIFEST_NAME))?;
    let manifest = parse_acpkg_manifest(&manifest_text)?;
    let mut asm = AcAssembler::new(dest_dir, manifest.files.clone(), cancel)?;
    if let Some(p) = progress {
        p.total_bytes.store(manifest.total_size, Ordering::Relaxed);
        p.done_bytes.store(asm.already_bytes, Ordering::Relaxed);
        p.files_total
            .store(manifest.files.len() as u64, Ordering::Relaxed);
        p.files_done.store(asm.skipped_resumed, Ordering::Relaxed);
        p.active_workers.store(1, Ordering::Relaxed);
        p.push_event(format!(
            ".acpkg verified: {} file(s), {} chunk(s)",
            manifest.files.len(),
            manifest.chunks.len()
        ));
    }
    let needed = asm.needed_chunk_indices(manifest.chunk_size, manifest.chunks.len())?;
    asm.finish_zero_byte_files()?;

    let mut read_bytes = 0u64;
    for chunk in &manifest.chunks {
        if cancel.load(Ordering::Relaxed) {
            if let Some(p) = progress {
                p.active_workers.store(0, Ordering::Relaxed);
            }
            return Err(StreamUnpackError::Cancelled);
        }
        if !needed.contains(&chunk.index) {
            continue;
        }
        let cpath = pkg_dir.join(AcManifest::chunk_name(chunk.index));
        let compressed = fs::read(&cpath)?;
        read_bytes += compressed.len() as u64;
        let actual_sha = hex::encode(Sha256::digest(&compressed));
        if !actual_sha.eq_ignore_ascii_case(&chunk.sha256) {
            return Err(StreamUnpackError::Integrity(format!(
                "chunk {} SHA-256 mismatch",
                chunk.index
            )));
        }
        let expected_len = manifest.chunk_expected_size(chunk.index);
        let raw = decompress_zstd_chunk(&compressed, expected_len)?;
        if raw.len() as u64 != expected_len {
            return Err(StreamUnpackError::Integrity(format!(
                "chunk {} size mismatch: expected {expected_len}, got {}",
                chunk.index,
                raw.len()
            )));
        }
        asm.feed_chunk(chunk.index as u64 * manifest.chunk_size, &raw)?;
        if let Some(p) = progress {
            p.done_bytes
                .store(asm.already_bytes + asm.bytes_written, Ordering::Relaxed);
            p.files_done
                .store(asm.files_done + asm.skipped_resumed, Ordering::Relaxed);
        }
        if delete_chunks {
            let _ = fs::remove_file(&cpath);
        }
    }
    if let Some(p) = progress {
        p.active_workers.store(0, Ordering::Relaxed);
    }
    Ok(asm.report(read_bytes))
}

/// Stream-download & unpack a remote HTTP/HTTPS `.acpkg` package with parallel chunk fetching,
/// per-chunk and per-file SHA-256 verification, and free resume.
pub async fn unpack_acpkg_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let http = UnpackHttp::single(client.clone());
    unpack_acpkg_url_with_http(&http, url, dest_dir, concurrency, cancel, progress).await
}

/// [`unpack_acpkg_url`] using an engine-configured [`UnpackHttp`] transport (headers, Multi-NIC,
/// automatic retries).
pub async fn unpack_acpkg_url_with_http(
    http: &UnpackHttp,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let (base_url, manifest_url) = if url.path().to_ascii_lowercase().ends_with("/manifest.json") {
        let mut base = url.clone();
        let trimmed = url.path().trim_end_matches("manifest.json");
        base.set_path(trimmed);
        (base, url.clone())
    } else {
        let mut base = url.clone();
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }
        let manifest_url = base
            .join(ACPKG_MANIFEST_NAME)
            .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
        (base, manifest_url)
    };

    let mut attempt = 0usize;
    let text = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let (_nic, req) = http.get(&manifest_url);
        match req.send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(t) => break t,
                Err(_) if attempt < http.max_retries => {
                    attempt += 1;
                    http.backoff(attempt, &cancel).await?;
                    continue;
                }
                Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
            },
            Ok(resp) if is_retryable_status(resp.status()) && attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Ok(resp) => {
                return Err(StreamUnpackError::Http(format!(
                    "manifest HTTP {}",
                    resp.status()
                )));
            }
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        }
    };
    let manifest = parse_acpkg_manifest(&text)?;
    let dest_owned = dest_dir.to_owned();
    let files_clone = manifest.files.clone();
    let cancel_init = cancel.clone();
    let mut asm = tokio::task::spawn_blocking(move || {
        AcAssembler::new(&dest_owned, files_clone, &cancel_init)
    })
    .await
    .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;
    if let Some(p) = &progress {
        p.total_bytes.store(manifest.total_size, Ordering::Relaxed);
        p.done_bytes.store(asm.already_bytes, Ordering::Relaxed);
        p.files_total
            .store(manifest.files.len() as u64, Ordering::Relaxed);
        p.files_done.store(asm.skipped_resumed, Ordering::Relaxed);
        p.push_event(format!(
            ".acpkg ready: {} file(s), {} resumed",
            manifest.files.len(),
            asm.skipped_resumed
        ));
    }
    let needed = asm.needed_chunk_indices(manifest.chunk_size, manifest.chunks.len())?;
    asm.finish_zero_byte_files()?;

    let to_fetch: Vec<AcChunkInfo> = manifest
        .chunks
        .iter()
        .filter(|c| needed.contains(&c.index))
        .cloned()
        .collect();

    let downloaded = Arc::new(AtomicU64::new(0));
    let window = concurrency.clamp(1, 8);
    if let Some(p) = &progress {
        p.active_workers
            .store(to_fetch.len().min(window) as u64, Ordering::Relaxed);
    }
    let mut stream = futures::stream::iter(to_fetch.into_iter().map(|chunk| {
        let http = http.clone();
        let base_url = base_url.clone();
        let downloaded = downloaded.clone();
        let cancel = cancel.clone();
        let progress = progress.clone();
        let expected_len = manifest.chunk_expected_size(chunk.index);
        async move {
            let cname = AcManifest::chunk_name(chunk.index);
            let curl = base_url
                .join(&cname)
                .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
            let mut attempt = 0usize;
            let bytes = loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(StreamUnpackError::Cancelled);
                }
                let (nic, req) = http.get(&curl);
                match req.send().await {
                    Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                        Ok(b) => {
                            let blen = b.len() as u64;
                            http.record_nic(progress.as_ref(), nic.as_deref(), blen);
                            break b;
                        }
                        Err(_) if attempt < http.max_retries => {
                            attempt += 1;
                            http.backoff(attempt, &cancel).await?;
                            continue;
                        }
                        Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
                    },
                    Ok(resp)
                        if is_retryable_status(resp.status()) && attempt < http.max_retries =>
                    {
                        attempt += 1;
                        http.backoff(attempt, &cancel).await?;
                        continue;
                    }
                    Ok(resp) => {
                        return Err(StreamUnpackError::Http(format!(
                            "{cname} HTTP {}",
                            resp.status()
                        )));
                    }
                    Err(_) if attempt < http.max_retries => {
                        attempt += 1;
                        http.backoff(attempt, &cancel).await?;
                        continue;
                    }
                    Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
                }
            };
            let blen = bytes.len() as u64;
            downloaded.fetch_add(blen, Ordering::Relaxed);
            pace_async(progress.as_ref(), blen, &cancel).await;
            let actual_sha = hex::encode(Sha256::digest(&bytes));
            if !actual_sha.eq_ignore_ascii_case(&chunk.sha256) {
                return Err(StreamUnpackError::Integrity(format!(
                    "{cname} SHA-256 mismatch"
                )));
            }
            let raw =
                tokio::task::spawn_blocking(move || decompress_zstd_chunk(&bytes, expected_len))
                    .await
                    .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;
            if raw.len() as u64 != expected_len {
                return Err(StreamUnpackError::Integrity(format!(
                    "{cname} decompressed length mismatch"
                )));
            }
            Ok((chunk.index, raw))
        }
    }))
    .buffered(window);

    while let Some(res) = stream.next().await {
        if cancel.load(Ordering::Relaxed) {
            if let Some(p) = &progress {
                p.active_workers.store(0, Ordering::Relaxed);
            }
            return Err(StreamUnpackError::Cancelled);
        }
        let (idx, raw) = res?;
        asm.feed_chunk(idx as u64 * manifest.chunk_size, &raw)?;
        if let Some(p) = &progress {
            p.done_bytes
                .store(asm.already_bytes + asm.bytes_written, Ordering::Relaxed);
            p.files_done
                .store(asm.files_done + asm.skipped_resumed, Ordering::Relaxed);
        }
    }
    if let Some(p) = &progress {
        p.active_workers.store(0, Ordering::Relaxed);
    }

    Ok(asm.report(downloaded.load(Ordering::Relaxed)))
}

// ============================================================================
// 2. Streaming HTTP `.zip` / ZIP64 (>300 GiB) Unpack (Constant RAM + Resume)
// ============================================================================

#[derive(Clone, Debug)]
struct ZipCdEntry {
    name: String,
    method: u16,
    crc32: u32,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoreSliceResume {
    start: u64,
    end: u64,
    done: u64,
    crc32: u32,
}

#[derive(Serialize, Deserialize, Default)]
struct ZipResumeState {
    total_size: u64,
    completed: HashSet<String>,
    /// Per-slice checkpoints for parallel multi-connection ZIP STORE entries (instant O(1) resume!).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    store_partials: BTreeMap<String, Vec<StoreSliceResume>>,
}

fn read_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn read_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn read_u64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

enum CdLookup {
    Found {
        cd_offset: u64,
        cd_size: u64,
    },
    /// ZIP64 EOCD record lies earlier than the fetched tail; caller fetches 56 bytes at this offset.
    NeedZip64Eocd(u64),
}

/// Locate Central Directory `(cd_offset, cd_size)` from a ZIP file's tail buffer (full ZIP64 support,
/// including archives >300 GiB and >65,535 entries where the ZIP64 EOCD sits before `tail_start`).
fn find_cd_bounds(tail: &[u8], tail_start: u64) -> Option<CdLookup> {
    if tail.len() < 22 {
        return None;
    }
    let mut eocd_pos = None;
    for i in (0..=tail.len() - 22).rev() {
        if &tail[i..i + 4] == b"PK\x05\x06" {
            let comment_len = read_u16_le(tail, i + 20) as usize;
            if i + 22 + comment_len <= tail.len() {
                eocd_pos = Some(i);
                break;
            }
        }
    }
    let pos = eocd_pos?;
    let total_entries_16 = read_u16_le(tail, pos + 10);
    let cd_size = u64::from(read_u32_le(tail, pos + 12));
    let cd_offset = u64::from(read_u32_le(tail, pos + 16));

    let need_z64 = cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF || total_entries_16 == 0xFFFF;
    if pos >= 20 && &tail[pos - 20..pos - 16] == b"PK\x06\x07" && need_z64 {
        let z64_off = read_u64_le(tail, pos - 12);
        if z64_off >= tail_start {
            let rel = (z64_off - tail_start) as usize;
            if rel + 56 <= tail.len() && &tail[rel..rel + 4] == b"PK\x06\x06" {
                return Some(CdLookup::Found {
                    cd_size: read_u64_le(tail, rel + 40),
                    cd_offset: read_u64_le(tail, rel + 48),
                });
            }
        }
        return Some(CdLookup::NeedZip64Eocd(z64_off));
    }
    Some(CdLookup::Found { cd_offset, cd_size })
}

fn parse_central_directory(cd: &[u8]) -> Result<Vec<ZipCdEntry>, StreamUnpackError> {
    let mut entries = Vec::new();
    let mut i = 0usize;
    while i + 46 <= cd.len() {
        if &cd[i..i + 4] != b"PK\x01\x02" {
            break;
        }
        let flags = read_u16_le(cd, i + 8);
        if flags & 1 != 0 {
            return Err(StreamUnpackError::Unsupported(
                "encrypted ZIP entries cannot be stream-unpacked".into(),
            ));
        }
        let method = read_u16_le(cd, i + 10);
        let crc32 = read_u32_le(cd, i + 16);
        let mut comp_size = u64::from(read_u32_le(cd, i + 20));
        let mut uncomp_size = u64::from(read_u32_le(cd, i + 24));
        let name_len = read_u16_le(cd, i + 28) as usize;
        let extra_len = read_u16_le(cd, i + 30) as usize;
        let comment_len = read_u16_le(cd, i + 32) as usize;
        let mut local_offset = u64::from(read_u32_le(cd, i + 42));

        let next = i
            .checked_add(46)
            .and_then(|v| v.checked_add(name_len))
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(|| StreamUnpackError::BadArchive("ZIP header length overflow".into()))?;
        if next > cd.len() {
            return Err(StreamUnpackError::BadArchive(
                "truncated ZIP central directory".into(),
            ));
        }
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + name_len]).into_owned();
        let extra = &cd[i + 46 + name_len..i + 46 + name_len + extra_len];

        if uncomp_size == 0xFFFF_FFFF || comp_size == 0xFFFF_FFFF || local_offset == 0xFFFF_FFFF {
            let mut ex = 0usize;
            while ex + 4 <= extra.len() {
                let tag = read_u16_le(extra, ex);
                let sz = read_u16_le(extra, ex + 2) as usize;
                let end = match ex.checked_add(4).and_then(|v| v.checked_add(sz)) {
                    Some(v) if v <= extra.len() => v,
                    _ => break,
                };
                if tag == 0x0001 {
                    let mut p = ex + 4;
                    if uncomp_size == 0xFFFF_FFFF && p + 8 <= end {
                        uncomp_size = read_u64_le(extra, p);
                        p += 8;
                    }
                    if comp_size == 0xFFFF_FFFF && p + 8 <= end {
                        comp_size = read_u64_le(extra, p);
                        p += 8;
                    }
                    if local_offset == 0xFFFF_FFFF && p + 8 <= end {
                        local_offset = read_u64_le(extra, p);
                    }
                    break;
                }
                ex = end;
            }
            if uncomp_size == 0xFFFF_FFFF || comp_size == 0xFFFF_FFFF || local_offset == 0xFFFF_FFFF
            {
                return Err(StreamUnpackError::BadArchive(format!(
                    "missing ZIP64 extra field for {name}"
                )));
            }
        }

        if !name.ends_with('/') && !name.ends_with('\\') {
            entries.push(ZipCdEntry {
                name,
                method,
                crc32,
                comp_size,
                uncomp_size,
                local_offset,
            });
        }
        i = next;
    }
    Ok(entries)
}

async fn fetch_range_http(
    http: &UnpackHttp,
    url: &Url,
    start: u64,
    end_inclusive: u64,
    cancel: &AtomicBool,
    progress: Option<&Arc<UnpackProgress>>,
) -> Result<Vec<u8>, StreamUnpackError> {
    if end_inclusive < start {
        return Ok(Vec::new());
    }
    let expected = (end_inclusive - start) + 1;
    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let (nic, req) = http.get(url);
        let send = req
            .header(
                reqwest::header::RANGE,
                format!("bytes={start}-{end_inclusive}"),
            )
            .send()
            .await;
        let resp = match send {
            Ok(r) => r,
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        };
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            if is_retryable_status(resp.status()) && attempt < http.max_retries {
                attempt += 1;
                http.backoff(attempt, cancel).await?;
                continue;
            }
            // Never buffer a full 200 OK response when requesting a partial range of a huge archive!
            if start > 0
                || resp
                    .content_length()
                    .is_none_or(|len| len > expected.saturating_add(65_536))
            {
                return Err(if resp.status().is_success() {
                    StreamUnpackError::Unsupported(
                        "server ignored HTTP Range request (200 OK instead of 206 Partial Content)"
                            .into(),
                    )
                } else {
                    StreamUnpackError::Http(format!("range HTTP {}", resp.status()))
                });
            }
        }
        match resp.bytes().await {
            Ok(bytes) => {
                http.record_nic(progress, nic.as_deref(), bytes.len() as u64);
                return Ok(bytes.to_vec());
            }
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        }
    }
}

/// Async-fed `Read` adapter backed by `tokio::sync::mpsc::Receiver` (never blocks Tokio workers!).
struct AsyncChannelReader {
    rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, String>>,
    buf: bytes::Bytes,
    pos: usize,
    cancel: Arc<AtomicBool>,
}

impl Read for AsyncChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        while self.pos >= self.buf.len() {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) if chunk.is_empty() => continue,
                Some(Ok(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Some(Err(e)) => return Err(io::Error::other(e)),
                None => return Ok(0),
            }
        }
        let avail = &self.buf[self.pos..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
}

enum SingleAttemptOutcome {
    Done(u64),
    Interrupted {
        written: u64,
        crc: crc32fast::Hasher,
        reason: String,
    },
}

/// Read the 30-byte ZIP Local File Header over HTTP Range to locate exact `[data_offset ..= data_end]`.
async fn resolve_zip_data_bounds(
    http: &UnpackHttp,
    url: &Url,
    entry: &ZipCdEntry,
    total_size: u64,
    downloaded: &Arc<AtomicU64>,
    cancel: &AtomicBool,
    progress: Option<&Arc<UnpackProgress>>,
) -> Result<(u64, u64), StreamUnpackError> {
    let hdr_end = entry
        .local_offset
        .checked_add(29)
        .filter(|&e| e < total_size)
        .ok_or_else(|| {
            StreamUnpackError::BadArchive(format!("invalid local header offset for {}", entry.name))
        })?;
    let lfh = fetch_range_http(http, url, entry.local_offset, hdr_end, cancel, progress).await?;
    downloaded.fetch_add(lfh.len() as u64, Ordering::Relaxed);
    if lfh.len() < 30 || &lfh[0..4] != b"PK\x03\x04" {
        return Err(StreamUnpackError::BadArchive(format!(
            "missing ZIP local header for {}",
            entry.name
        )));
    }
    let l_nlen = u64::from(read_u16_le(&lfh, 26));
    let l_elen = u64::from(read_u16_le(&lfh, 28));
    let data_offset = entry
        .local_offset
        .checked_add(30)
        .and_then(|v| v.checked_add(l_nlen))
        .and_then(|v| v.checked_add(l_elen))
        .ok_or_else(|| StreamUnpackError::BadArchive("ZIP data offset overflow".into()))?;
    let data_end = data_offset
        .checked_add(entry.comp_size - 1)
        .filter(|&e| e < total_size)
        .ok_or_else(|| {
            StreamUnpackError::BadArchive(format!("ZIP entry {} exceeds archive size", entry.name))
        })?;
    Ok((data_offset, data_end))
}

/// Stream-unpack a single ZIP entry in constant RAM (<512 KiB) even for 300+ GB entries.
/// - Supports method 0 (Store), 8 (Deflate), and 93 (Zstd).
/// - Supports **mid-entry byte-level resume** AND **automatic mid-stream reconnect** for STORE (`method == 0`).
/// - Automatically retries transient network / HTTP 5xx errors up to `http.max_retries`.
#[allow(clippy::too_many_arguments)]
async fn stream_zip_entry(
    http: &UnpackHttp,
    url: &Url,
    entry: &ZipCdEntry,
    total_size: u64,
    dest_dir: &Path,
    downloaded: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
    progress: Option<&Arc<UnpackProgress>>,
) -> Result<u64, StreamUnpackError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(StreamUnpackError::Cancelled);
    }
    let rel = safe::rel_path(&entry.name, 64).map_err(|w| {
        StreamUnpackError::BadArchive(format!("unsafe ZIP path {}: {w}", entry.name))
    })?;
    let dst = dest_dir.join(&rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    if entry.comp_size == 0 && entry.uncomp_size == 0 {
        if entry.crc32 != 0 {
            return Err(StreamUnpackError::Integrity(format!(
                "non-zero CRC32 on empty ZIP entry {}",
                entry.name
            )));
        }
        let tmp = PathBuf::from(format!("{}.acpart", dst.display()));
        File::create(&tmp)?;
        if dst.exists() {
            let _ = fs::remove_file(&dst);
        }
        fs::rename(&tmp, &dst)?;
        return Ok(0);
    }

    if !matches!(entry.method, 0 | 8 | 93) {
        return Err(StreamUnpackError::Unsupported(format!(
            "ZIP compression method {} for {}",
            entry.method, entry.name
        )));
    }

    let (data_offset, data_end) =
        resolve_zip_data_bounds(http, url, entry, total_size, downloaded, cancel, progress).await?;
    let tmp = PathBuf::from(format!("{}.acpart", dst.display()));

    // For STORE (method 0), resume mid-entry from an existing `.acpart` file if present!
    let mut initial_resumed = 0u64;
    let mut resume_offset = 0u64;
    let mut running_crc = crc32fast::Hasher::new();
    if entry.method == 0
        && let Ok(meta) = fs::metadata(&tmp)
        && meta.is_file()
        && meta.len() > 0
        && meta.len() <= entry.uncomp_size
    {
        let existing_len = meta.len();
        let tmp_check = tmp.clone();
        let cancel_check = cancel.clone();
        let hashed =
            tokio::task::spawn_blocking(move || -> Result<crc32fast::Hasher, StreamUnpackError> {
                let mut f = File::open(&tmp_check)?;
                let mut h = crc32fast::Hasher::new();
                let mut buf = vec![0u8; IO_BUF_SIZE];
                let mut rem = existing_len;
                while rem > 0 {
                    if cancel_check.load(Ordering::Relaxed) {
                        return Err(StreamUnpackError::Cancelled);
                    }
                    let want = (rem as usize).min(buf.len());
                    let n = f.read(&mut buf[..want])?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                    rem -= n as u64;
                }
                Ok(h)
            })
            .await
            .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;

        if existing_len == entry.uncomp_size {
            if hashed.finalize() == entry.crc32 {
                if let Some(p) = progress {
                    p.done_bytes.fetch_add(existing_len, Ordering::Relaxed);
                }
                if dst.exists() {
                    let _ = fs::remove_file(&dst);
                }
                fs::rename(&tmp, &dst)?;
                return Ok(existing_len);
            }
            let _ = fs::remove_file(&tmp);
        } else {
            initial_resumed = existing_len;
            resume_offset = existing_len;
            running_crc = hashed;
            if let Some(p) = progress {
                p.done_bytes.fetch_add(resume_offset, Ordering::Relaxed);
            }
        }
    }

    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let req_start = data_offset + resume_offset;
        let (nic, req) = http.get(url);
        let resp = match req
            .header(
                reqwest::header::RANGE,
                format!("bytes={req_start}-{data_end}"),
            )
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                if let Some(p) = progress {
                    p.push_event(format!(
                        "Reconnecting {} (attempt {attempt}/{})",
                        entry.name, http.max_retries
                    ));
                }
                http.backoff(attempt, cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        };
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status().is_success() {
            if is_retryable_status(resp.status()) && attempt < http.max_retries {
                attempt += 1;
                http.backoff(attempt, cancel).await?;
                continue;
            }
            return Err(StreamUnpackError::Http(format!(
                "entry range HTTP {} for {}",
                resp.status(),
                entry.name
            )));
        }
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && req_start > 0 {
            return Err(StreamUnpackError::Unsupported(
                "server ignored HTTP Range request on ZIP entry".into(),
            ));
        }
        let append_mode = resume_offset > 0;

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(16);
        let method = entry.method;
        let expected_crc = entry.crc32;
        let expected_uncomp = entry.uncomp_size;
        let entry_name = entry.name.clone();
        let tmp_worker = tmp.clone();
        let dst_worker = dst.clone();
        let cancel_worker = cancel.clone();
        let prog_worker = progress.cloned();
        let start_offset = resume_offset;
        let crc_for_worker = running_crc.clone();

        let writer_task = tokio::task::spawn_blocking(
            move || -> Result<SingleAttemptOutcome, StreamUnpackError> {
                let reader = AsyncChannelReader {
                    rx,
                    buf: bytes::Bytes::new(),
                    pos: 0,
                    cancel: cancel_worker.clone(),
                };
                let file = if append_mode {
                    OpenOptions::new().append(true).open(&tmp_worker)?
                } else {
                    File::create(&tmp_worker)?
                };
                let mut out = BufWriter::with_capacity(IO_BUF_SIZE, file);
                let mut crc = crc_for_worker;
                let mut written = start_offset;

                let mut dec: Box<dyn Read> = match method {
                    0 => Box::new(reader),
                    8 => Box::new(flate2::read::DeflateDecoder::new(reader)),
                    93 => Box::new(ruzstd::StreamingDecoder::new(reader).map_err(|e| {
                        StreamUnpackError::BadArchive(format!(
                            "ZIP zstd error for {entry_name}: {e}"
                        ))
                    })?),
                    other => {
                        return Err(StreamUnpackError::Unsupported(format!(
                            "ZIP compression method {other} for {entry_name}"
                        )));
                    }
                };

                let mut buf = vec![0u8; IO_BUF_SIZE];
                let mut stream_err: Option<String> = None;
                loop {
                    if cancel_worker.load(Ordering::Relaxed) {
                        let _ = out.flush();
                        drop(out);
                        if method != 0 {
                            let _ = fs::remove_file(&tmp_worker);
                        }
                        return Err(StreamUnpackError::Cancelled);
                    }
                    let n = match dec.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                            let _ = out.flush();
                            drop(out);
                            if method != 0 {
                                let _ = fs::remove_file(&tmp_worker);
                            }
                            return Err(StreamUnpackError::Cancelled);
                        }
                        Err(e) => {
                            stream_err = Some(e.to_string());
                            break;
                        }
                    };
                    let next_written = written.checked_add(n as u64).ok_or_else(|| {
                        StreamUnpackError::BadArchive("ZIP output size overflow".into())
                    })?;
                    if next_written > expected_uncomp {
                        drop(out);
                        let _ = fs::remove_file(&tmp_worker);
                        return Err(StreamUnpackError::Integrity(format!(
                            "ZIP entry {entry_name} decompressed beyond declared size ({expected_uncomp} bytes)"
                        )));
                    }
                    out.write_all(&buf[..n])?;
                    crc.update(&buf[..n]);
                    written = next_written;
                    if let Some(p) = &prog_worker {
                        p.done_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }

                out.flush()?;
                drop(out);

                if let Some(reason) = stream_err {
                    if method != 0 {
                        let _ = fs::remove_file(&tmp_worker);
                    }
                    return Ok(SingleAttemptOutcome::Interrupted {
                        written,
                        crc,
                        reason,
                    });
                }
                if written != expected_uncomp {
                    if method != 0 {
                        let _ = fs::remove_file(&tmp_worker);
                    }
                    return Ok(SingleAttemptOutcome::Interrupted {
                        written,
                        crc,
                        reason: format!(
                            "truncated ZIP entry {entry_name}: expected {expected_uncomp} bytes, got {written}"
                        ),
                    });
                }
                let got_crc = crc.finalize();
                if got_crc != expected_crc {
                    let _ = fs::remove_file(&tmp_worker);
                    return Err(StreamUnpackError::Integrity(format!(
                        "CRC32 mismatch on {entry_name}: expected {expected_crc:08x}, got {got_crc:08x}"
                    )));
                }
                if dst_worker.exists() {
                    let _ = fs::remove_file(&dst_worker);
                }
                fs::rename(&tmp_worker, &dst_worker)?;
                Ok(SingleAttemptOutcome::Done(written))
            },
        );

        let mut body = resp.bytes_stream();
        let mut comp_left = (data_end - req_start) + 1;
        while let Some(chunk) = body.next().await {
            if cancel.load(Ordering::Relaxed) {
                drop(tx);
                let _ = writer_task.await;
                return Err(StreamUnpackError::Cancelled);
            }
            match chunk {
                Ok(mut b) => {
                    if b.is_empty() {
                        continue;
                    }
                    if (b.len() as u64) > comp_left {
                        b = b.slice(..comp_left as usize);
                    }
                    let len = b.len() as u64;
                    downloaded.fetch_add(len, Ordering::Relaxed);
                    http.record_nic(progress, nic.as_deref(), len);
                    comp_left -= len;
                    pace_async(progress, len, cancel).await;
                    if tx.send(Ok(b)).await.is_err() {
                        break;
                    }
                    if comp_left == 0 {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
        drop(tx);

        match writer_task
            .await
            .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??
        {
            SingleAttemptOutcome::Done(total_written) => {
                return Ok(total_written.saturating_sub(initial_resumed));
            }
            SingleAttemptOutcome::Interrupted {
                written,
                crc,
                reason,
            } => {
                if attempt < http.max_retries && !cancel.load(Ordering::Relaxed) {
                    attempt += 1;
                    if entry.method == 0 {
                        resume_offset = written;
                        running_crc = crc;
                    } else {
                        if let Some(p) = progress {
                            p.done_bytes.fetch_sub(
                                written.saturating_sub(resume_offset),
                                Ordering::Relaxed,
                            );
                        }
                        resume_offset = 0;
                        running_crc = crc32fast::Hasher::new();
                    }
                    if let Some(p) = progress {
                        p.push_event(format!(
                            "Resuming {} after drop (attempt {attempt}/{}): {reason}",
                            entry.name, http.max_retries
                        ));
                    }
                    http.backoff(attempt, cancel).await?;
                    continue;
                }
                return Err(StreamUnpackError::Http(reason));
            }
        }
    }
}

/// Multi-connection parallel streaming for huge ZIP STORE (`method == 0`) entries:
/// - Splits a single huge file (e.g. 250 GB `.pak`) across `window` concurrent Range connections
///   (and Multi-NIC adapters when configured).
/// - Checkpoints per-slice `(done, crc32)` into `.anticompress-resume.json` for **O(1) instant
///   pause/resume** without re-reading gigabytes from disk.
/// - Recombines per-slice CRC32s in O(1) via `crc32fast::Hasher::combine` with zero extra disk I/O.
#[allow(clippy::too_many_arguments)]
async fn stream_zip_store_parallel(
    http: &UnpackHttp,
    url: &Url,
    entry: &ZipCdEntry,
    total_size: u64,
    dest_dir: &Path,
    window: usize,
    resume_path: &Path,
    resume_state: &Arc<Mutex<ZipResumeState>>,
    downloaded: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
    progress: Option<&Arc<UnpackProgress>>,
) -> Result<u64, StreamUnpackError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(StreamUnpackError::Cancelled);
    }
    let rel = safe::rel_path(&entry.name, 64).map_err(|w| {
        StreamUnpackError::BadArchive(format!("unsafe ZIP path {}: {w}", entry.name))
    })?;
    let dst = dest_dir.join(&rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let (data_offset, _data_end) =
        resolve_zip_data_bounds(http, url, entry, total_size, downloaded, cancel, progress).await?;
    let tmp = PathBuf::from(format!("{}.acpart", dst.display()));

    // Check if we can O(1) resume from saved slice checkpoints + matching pre-sized `.acpart`.
    let saved_slices = resume_state
        .lock()
        .ok()
        .and_then(|g| g.store_partials.get(&entry.name).cloned())
        .filter(|slices| {
            !slices.is_empty()
                && fs::metadata(&tmp)
                    .map(|m| m.is_file() && m.len() == entry.uncomp_size)
                    .unwrap_or(false)
                && slices.first().is_some_and(|s| s.start == 0)
                && slices
                    .last()
                    .is_some_and(|s| s.end + 1 == entry.uncomp_size)
                && slices.windows(2).all(|w| w[0].end + 1 == w[1].start)
                && slices.iter().all(|s| s.done <= (s.end - s.start) + 1)
        });

    let initial_slices: Vec<StoreSliceResume> = if let Some(slices) = saved_slices {
        let already: u64 = slices.iter().map(|s| s.done).sum();
        if let Some(p) = progress {
            p.done_bytes.fetch_add(already, Ordering::Relaxed);
            p.push_event(format!(
                "Instant resume ({} streams, {already} B kept): {}",
                slices.len(),
                entry.name
            ));
        }
        slices
    } else {
        let f = File::create(&tmp)?;
        f.set_len(entry.uncomp_size)?;
        let count = (window.clamp(2, 8) as u64).min(entry.uncomp_size);
        let base = entry.uncomp_size / count;
        let rem = entry.uncomp_size % count;
        let mut slices = Vec::with_capacity(count as usize);
        let mut cursor = 0u64;
        for idx in 0..count {
            let len = base + u64::from(idx < rem);
            slices.push(StoreSliceResume {
                start: cursor,
                end: cursor + len - 1,
                done: 0,
                crc32: 0,
            });
            cursor += len;
        }
        if let Ok(mut guard) = resume_state.lock() {
            guard
                .store_partials
                .insert(entry.name.clone(), slices.clone());
            if let Ok(text) = serde_json::to_string(&*guard) {
                let _ = write_atomic(resume_path, &text);
            }
        }
        if let Some(p) = progress {
            p.push_event(format!("Parallel STORE ({count} streams): {}", entry.name));
        }
        slices
    };

    let active = initial_slices
        .iter()
        .filter(|s| s.done < (s.end - s.start) + 1)
        .count();
    if let Some(p) = progress {
        p.active_workers.store(active as u64, Ordering::Relaxed);
    }

    let checkpoint_slice = |state: &Arc<Mutex<ZipResumeState>>,
                            rpath: &Path,
                            ename: &str,
                            idx: usize,
                            done: u64,
                            crc32: u32| {
        if let Ok(mut guard) = state.lock()
            && let Some(vec) = guard.store_partials.get_mut(ename)
            && let Some(sl) = vec.get_mut(idx)
        {
            sl.done = done;
            sl.crc32 = crc32;
            if let Ok(text) = serde_json::to_string(&*guard) {
                let _ = write_atomic(rpath, &text);
            }
        }
    };

    let futs = initial_slices
        .into_iter()
        .enumerate()
        .map(|(slice_idx, mut slice)| {
            let http = http.clone();
            let url = url.clone();
            let tmp = tmp.clone();
            let ename = entry.name.clone();
            let rpath = resume_path.to_owned();
            let rstate = resume_state.clone();
            let downloaded = downloaded.clone();
            let cancel = cancel.clone();
            let progress = progress.cloned();
            async move {
                let slice_len = (slice.end - slice.start) + 1;
                let initial_done = slice.done;
                let mut attempt = 0usize;
                while slice.done < slice_len {
                    if cancel.load(Ordering::Relaxed) {
                        checkpoint_slice(
                            &rstate,
                            &rpath,
                            &ename,
                            slice_idx,
                            slice.done,
                            slice.crc32,
                        );
                        return Err(StreamUnpackError::Cancelled);
                    }
                    let req_start = data_offset + slice.start + slice.done;
                    let req_end = data_offset + slice.end;
                    let (nic, req) = http.get(&url);
                    let resp = match req
                        .header(
                            reqwest::header::RANGE,
                            format!("bytes={req_start}-{req_end}"),
                        )
                        .send()
                        .await
                    {
                        Ok(r) => r,
                        Err(_) if attempt < http.max_retries => {
                            attempt += 1;
                            http.backoff(attempt, &cancel).await?;
                            continue;
                        }
                        Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
                    };
                    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                        if is_retryable_status(resp.status()) && attempt < http.max_retries {
                            attempt += 1;
                            http.backoff(attempt, &cancel).await?;
                            continue;
                        }
                        return Err(if resp.status().is_success() {
                            StreamUnpackError::Unsupported(
                                "server ignored HTTP Range request on parallel ZIP STORE slice"
                                    .into(),
                            )
                        } else {
                            StreamUnpackError::Http(format!(
                                "slice HTTP {} for {ename}",
                                resp.status()
                            ))
                        });
                    }

                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(16);
                    let tmp_w = tmp.clone();
                    let cancel_w = cancel.clone();
                    let prog_w = progress.clone();
                    let rstate_w = rstate.clone();
                    let rpath_w = rpath.clone();
                    let ename_w = ename.clone();
                    let file_offset = slice.start + slice.done;
                    let start_done = slice.done;
                    let start_crc = slice.crc32;

                    let writer = tokio::task::spawn_blocking(
                        move || -> Result<(u64, u32, Option<String>), StreamUnpackError> {
                            let mut reader = AsyncChannelReader {
                                rx,
                                buf: bytes::Bytes::new(),
                                pos: 0,
                                cancel: cancel_w.clone(),
                            };
                            let mut f = OpenOptions::new().write(true).open(&tmp_w)?;
                            f.seek(SeekFrom::Start(file_offset))?;
                            let mut out = BufWriter::with_capacity(IO_BUF_SIZE, f);
                            let mut crc = crc32fast::Hasher::new_with_initial(start_crc);
                            let mut done = start_done;
                            let mut since_ckpt = 0u64;
                            let mut buf = vec![0u8; IO_BUF_SIZE];
                            let mut stream_err = None;
                            const CKPT_EVERY: u64 = 8 * 1024 * 1024;

                            loop {
                                if cancel_w.load(Ordering::Relaxed) {
                                    let _ = out.flush();
                                    let c = crc.finalize();
                                    checkpoint_slice(
                                        &rstate_w, &rpath_w, &ename_w, slice_idx, done, c,
                                    );
                                    return Err(StreamUnpackError::Cancelled);
                                }
                                let n = match reader.read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => n,
                                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                                        let _ = out.flush();
                                        let c = crc.finalize();
                                        checkpoint_slice(
                                            &rstate_w, &rpath_w, &ename_w, slice_idx, done, c,
                                        );
                                        return Err(StreamUnpackError::Cancelled);
                                    }
                                    Err(e) => {
                                        stream_err = Some(e.to_string());
                                        break;
                                    }
                                };
                                let rem = slice_len.saturating_sub(done) as usize;
                                let take = n.min(rem);
                                if take == 0 {
                                    break;
                                }
                                out.write_all(&buf[..take])?;
                                crc.update(&buf[..take]);
                                done += take as u64;
                                since_ckpt += take as u64;
                                if let Some(p) = &prog_w {
                                    p.done_bytes.fetch_add(take as u64, Ordering::Relaxed);
                                }
                                if since_ckpt >= CKPT_EVERY {
                                    out.flush()?;
                                    checkpoint_slice(
                                        &rstate_w,
                                        &rpath_w,
                                        &ename_w,
                                        slice_idx,
                                        done,
                                        crc.clone().finalize(),
                                    );
                                    since_ckpt = 0;
                                }
                            }
                            out.flush()?;
                            let final_crc = crc.finalize();
                            checkpoint_slice(
                                &rstate_w, &rpath_w, &ename_w, slice_idx, done, final_crc,
                            );
                            Ok((done, final_crc, stream_err))
                        },
                    );

                    let mut body = resp.bytes_stream();
                    let mut left = (req_end - req_start) + 1;
                    while let Some(chunk) = body.next().await {
                        if cancel.load(Ordering::Relaxed) {
                            drop(tx);
                            let _ = writer.await;
                            return Err(StreamUnpackError::Cancelled);
                        }
                        match chunk {
                            Ok(mut b) => {
                                if b.is_empty() {
                                    continue;
                                }
                                if (b.len() as u64) > left {
                                    b = b.slice(..left as usize);
                                }
                                let len = b.len() as u64;
                                downloaded.fetch_add(len, Ordering::Relaxed);
                                http.record_nic(progress.as_ref(), nic.as_deref(), len);
                                left -= len;
                                pace_async(progress.as_ref(), len, &cancel).await;
                                if tx.send(Ok(b)).await.is_err() {
                                    break;
                                }
                                if left == 0 {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(Err(e.to_string())).await;
                                break;
                            }
                        }
                    }
                    drop(tx);

                    let (new_done, new_crc, err_opt) = writer
                        .await
                        .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;
                    slice.done = new_done;
                    slice.crc32 = new_crc;

                    if slice.done < slice_len {
                        if attempt < http.max_retries && !cancel.load(Ordering::Relaxed) {
                            attempt += 1;
                            if let Some(p) = &progress {
                                p.push_event(format!(
                                    "Reconnecting {ename} slice {} (attempt {attempt}/{})",
                                    slice_idx + 1,
                                    http.max_retries
                                ));
                            }
                            http.backoff(attempt, &cancel).await?;
                            continue;
                        }
                        return Err(StreamUnpackError::Http(err_opt.unwrap_or_else(|| {
                            format!(
                                "truncated slice {} of {ename} ({}/{slice_len})",
                                slice_idx + 1,
                                slice.done
                            )
                        })));
                    }
                }
                Ok::<(u32, u64, u64), StreamUnpackError>((
                    slice.crc32,
                    slice_len,
                    slice.done.saturating_sub(initial_done),
                ))
            }
        });

    let results = futures::future::try_join_all(futs).await?;
    let mut combined = crc32fast::Hasher::new();
    let mut newly_written = 0u64;
    for (slice_crc, slice_len, wrote) in results {
        combined.combine(&crc32fast::Hasher::new_with_initial_len(
            slice_crc, slice_len,
        ));
        newly_written = newly_written.saturating_add(wrote);
    }
    let got_crc = combined.finalize();
    if let Ok(mut guard) = resume_state.lock() {
        guard.store_partials.remove(&entry.name);
        if let Ok(text) = serde_json::to_string(&*guard) {
            let _ = write_atomic(resume_path, &text);
        }
    }
    if got_crc != entry.crc32 {
        let _ = fs::remove_file(&tmp);
        return Err(StreamUnpackError::Integrity(format!(
            "CRC32 mismatch on parallel STORE {}: expected {:08x}, got {got_crc:08x}",
            entry.name, entry.crc32
        )));
    }
    if dst.exists() {
        let _ = fs::remove_file(&dst);
    }
    fs::rename(&tmp, &dst)?;
    Ok(newly_written)
}

/// Stream-unpack a remote `.zip` (including ZIP64 >300 GiB) over HTTP `Range` in parallel:
/// 1× disk space, constant RAM per worker, CRC32 & size verified, atomic pause/resume via
/// `.anticompress-resume.json` (plus parallel multi-connection slicing for large STORE entries).
pub async fn unpack_zip_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let http = UnpackHttp::single(client.clone());
    unpack_zip_url_with_http(&http, url, dest_dir, concurrency, cancel, progress).await
}

/// [`unpack_zip_url`] using an engine-configured [`UnpackHttp`] transport.
pub async fn unpack_zip_url_with_http(
    http: &UnpackHttp,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    fs::create_dir_all(dest_dir)?;

    // Determine archive length via HEAD (with retries), falling back to a 1-byte Range probe.
    let mut head_len = None;
    let mut attempt = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let (_nic, req) = http.head(url);
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                head_len = resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n >= 22);
                break;
            }
            Ok(resp) if is_retryable_status(resp.status()) && attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Ok(_) => break, // 405 Method Not Allowed etc. — fall through to Range probe
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        }
    }

    let total_size = if let Some(len) = head_len {
        len
    } else {
        let mut probe_attempt = 0usize;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(StreamUnpackError::Cancelled);
            }
            let (_nic, req) = http.get(url);
            match req.header(reqwest::header::RANGE, "bytes=0-0").send().await {
                Ok(probe)
                    if is_retryable_status(probe.status()) && probe_attempt < http.max_retries =>
                {
                    probe_attempt += 1;
                    http.backoff(probe_attempt, &cancel).await?;
                    continue;
                }
                Ok(probe) => {
                    break probe
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.rsplit('/').next())
                        .and_then(|s| s.parse::<u64>().ok())
                        .filter(|&n| n >= 22)
                        .ok_or_else(|| {
                            StreamUnpackError::Unsupported(
                                "server did not announce ZIP size / Range support".into(),
                            )
                        })?;
                }
                Err(_) if probe_attempt < http.max_retries => {
                    probe_attempt += 1;
                    http.backoff(probe_attempt, &cancel).await?;
                    continue;
                }
                Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
            }
        }
    };

    let tail_len = total_size.min(65_536);
    let tail_start = total_size - tail_len;
    let tail = fetch_range_http(
        http,
        url,
        tail_start,
        total_size - 1,
        &cancel,
        progress.as_ref(),
    )
    .await?;
    let downloaded = Arc::new(AtomicU64::new(tail.len() as u64));

    let (cd_offset, cd_size) = match find_cd_bounds(&tail, tail_start)
        .ok_or_else(|| StreamUnpackError::BadArchive("ZIP EOCD not found in tail".into()))?
    {
        CdLookup::Found { cd_offset, cd_size } => (cd_offset, cd_size),
        CdLookup::NeedZip64Eocd(z64_off) => {
            let z64_end = z64_off
                .checked_add(55)
                .filter(|&e| e < total_size)
                .ok_or_else(|| StreamUnpackError::BadArchive("invalid ZIP64 EOCD offset".into()))?;
            let z64 =
                fetch_range_http(http, url, z64_off, z64_end, &cancel, progress.as_ref()).await?;
            downloaded.fetch_add(z64.len() as u64, Ordering::Relaxed);
            if z64.len() < 56 || &z64[0..4] != b"PK\x06\x06" {
                return Err(StreamUnpackError::BadArchive(
                    "missing ZIP64 EOCD record".into(),
                ));
            }
            (read_u64_le(&z64, 48), read_u64_le(&z64, 40))
        }
    };

    let cd_end = cd_offset
        .checked_add(cd_size)
        .filter(|&end| cd_size > 0 && cd_size <= MAX_CD_BYTES && end <= total_size)
        .ok_or_else(|| {
            StreamUnpackError::BadArchive("invalid ZIP central directory bounds".into())
        })?;

    let cd_bytes = if cd_offset >= tail_start {
        let rel = (cd_offset - tail_start) as usize;
        tail[rel..rel + cd_size as usize].to_vec()
    } else {
        let bytes =
            fetch_range_http(http, url, cd_offset, cd_end - 1, &cancel, progress.as_ref()).await?;
        downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        bytes
    };

    let entries = parse_central_directory(&cd_bytes)?;
    let total_uncomp: u64 = entries
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.uncomp_size));
    let total_files = entries.len() as u64;
    let resume_path = dest_dir.join(RESUME_FILE_NAME);
    let mut resume = fs::read_to_string(&resume_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ZipResumeState>(&s).ok())
        .filter(|r| r.total_size == total_size)
        .unwrap_or(ZipResumeState {
            total_size,
            completed: HashSet::new(),
            store_partials: BTreeMap::new(),
        });

    let mut pending = Vec::new();
    let mut skipped_resumed = 0u64;
    let mut already_bytes = 0u64;
    for entry in entries {
        if resume.completed.contains(&entry.name) {
            let ok = safe::rel_path(&entry.name, 64)
                .ok()
                .and_then(|r| fs::metadata(dest_dir.join(r)).ok())
                .is_some_and(|m| m.is_file() && m.len() == entry.uncomp_size);
            if ok {
                skipped_resumed += 1;
                already_bytes = already_bytes.saturating_add(entry.uncomp_size);
                continue;
            }
            resume.completed.remove(&entry.name);
        }
        pending.push(entry);
    }

    if let Some(p) = &progress {
        p.total_bytes
            .store(total_uncomp.max(total_size), Ordering::Relaxed);
        p.done_bytes.store(already_bytes, Ordering::Relaxed);
        p.files_total.store(total_files, Ordering::Relaxed);
        p.files_done.store(skipped_resumed, Ordering::Relaxed);
        p.push_event(format!(
            "ZIP catalog loaded: {total_files} file(s), {skipped_resumed} resumed"
        ));
    }

    let resume_state = Arc::new(Mutex::new(resume));
    let bytes_written = Arc::new(AtomicU64::new(0));
    let files_done = Arc::new(AtomicU64::new(0));
    let window = concurrency.clamp(1, 8);

    // Split huge STORE entries so each gets full `window` parallel Range streams (and Multi-NIC!),
    // while smaller/compressed entries run `window`-wide concurrently.
    let mut regular = Vec::new();
    let mut large_store = Vec::new();
    for entry in pending {
        if entry.method == 0
            && window > 1
            && entry.uncomp_size >= http.parallel_store_threshold
            && entry.uncomp_size >= window as u64
        {
            large_store.push(entry);
        } else {
            regular.push(entry);
        }
    }

    if !regular.is_empty() {
        if let Some(p) = &progress {
            p.active_workers
                .store(regular.len().min(window) as u64, Ordering::Relaxed);
        }
        let mut stream = futures::stream::iter(regular.into_iter().map(|entry| {
            let http = http.clone();
            let url = url.clone();
            let dest_dir = dest_dir.to_owned();
            let resume_path = resume_path.clone();
            let resume_state = resume_state.clone();
            let downloaded = downloaded.clone();
            let bytes_written = bytes_written.clone();
            let files_done = files_done.clone();
            let cancel = cancel.clone();
            let progress = progress.clone();
            async move {
                let wrote = stream_zip_entry(
                    &http,
                    &url,
                    &entry,
                    total_size,
                    &dest_dir,
                    &downloaded,
                    &cancel,
                    progress.as_ref(),
                )
                .await?;
                bytes_written.fetch_add(wrote, Ordering::Relaxed);
                files_done.fetch_add(1, Ordering::Relaxed);
                if let Some(p) = &progress {
                    p.files_done.fetch_add(1, Ordering::Relaxed);
                }

                if let Ok(mut guard) = resume_state.lock() {
                    guard.completed.insert(entry.name);
                    if let Ok(text) = serde_json::to_string(&*guard) {
                        let _ = write_atomic(&resume_path, &text);
                    }
                }
                Ok::<(), StreamUnpackError>(())
            }
        }))
        .buffer_unordered(window);

        let mut first_err = None;
        while let Some(res) = stream.next().await {
            if let Err(e) = res {
                first_err = Some(e);
                break;
            }
        }
        if let Some(e) = first_err {
            if let Some(p) = &progress {
                p.active_workers.store(0, Ordering::Relaxed);
            }
            return Err(e);
        }
    }

    for entry in large_store {
        let wrote = match stream_zip_store_parallel(
            http,
            url,
            &entry,
            total_size,
            dest_dir,
            window,
            &resume_path,
            &resume_state,
            &downloaded,
            &cancel,
            progress.as_ref(),
        )
        .await
        {
            Ok(w) => w,
            Err(e) => {
                if let Some(p) = &progress {
                    p.active_workers.store(0, Ordering::Relaxed);
                }
                return Err(e);
            }
        };
        bytes_written.fetch_add(wrote, Ordering::Relaxed);
        files_done.fetch_add(1, Ordering::Relaxed);
        if let Some(p) = &progress {
            p.files_done.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut guard) = resume_state.lock() {
            guard.completed.insert(entry.name);
            if let Ok(text) = serde_json::to_string(&*guard) {
                let _ = write_atomic(&resume_path, &text);
            }
        }
    }

    if let Some(p) = &progress {
        p.active_workers.store(0, Ordering::Relaxed);
    }
    let _ = fs::remove_file(&resume_path);

    Ok(StreamUnpackReport {
        files: files_done.load(Ordering::Relaxed),
        bytes_written: bytes_written.load(Ordering::Relaxed),
        bytes_downloaded: downloaded.load(Ordering::Relaxed),
        dest_dir: dest_dir.to_owned(),
        skipped_resumed,
    })
}

// ============================================================================
// 3. Streaming HTTP `.tar` / `.tar.gz` / `.tar.xz` / `.tar.zst` (1× Disk Space)
// ============================================================================

#[derive(Serialize, Deserialize, Default)]
struct TarResumeState {
    total_size: u64,
    /// Exact 512-byte aligned offset of the next unread tar entry header (plain `.tar` only).
    next_offset: u64,
    files_done: u64,
    bytes_written: u64,
}

struct PrefixedReader<R> {
    head: Vec<u8>,
    pos: usize,
    inner: R,
}

impl<R: Read> Read for PrefixedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.head.len() {
            let n = (self.head.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(out)
    }
}

#[allow(clippy::too_many_arguments)]
fn unpack_tar_reader<R: Read>(
    reader: R,
    dest_dir: &Path,
    cancel: &AtomicBool,
    progress: Option<&Arc<UnpackProgress>>,
    plain_resume: Option<(&Path, u64, u64, u64, u64)>, // (resume_path, total_size, base_offset, prior_files, prior_bytes)
) -> Result<(u64, u64, u64), StreamUnpackError> {
    fs::create_dir_all(dest_dir)?;
    let mut archive = tar::Archive::new(reader);
    let mut files = 0u64;
    let mut skipped_resumed = 0u64;
    let mut bytes = 0u64;
    let mut buf = vec![0u8; IO_BUF_SIZE];

    for entry in archive.entries()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let mut entry = entry?;
        let raw_header_pos = entry.raw_header_position();
        let entry_size = entry.header().size().unwrap_or(0);
        let etype = entry.header().entry_type();
        if etype.is_dir() {
            let raw_path = entry.path()?.to_string_lossy().into_owned();
            if let Ok(rel) = safe::rel_path(&raw_path, 64) {
                let _ = fs::create_dir_all(dest_dir.join(rel));
            }
            continue;
        }
        if !(etype.is_file() || etype.is_contiguous()) {
            continue;
        }
        let raw_path = entry.path()?.to_string_lossy().into_owned();
        let rel = safe::rel_path(&raw_path, 64).map_err(|w| {
            StreamUnpackError::BadArchive(format!("unsafe tar path {raw_path}: {w}"))
        })?;
        let dst = dest_dir.join(rel);
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }

        // For compressed `.tar.*` retries, skip writing entries already completely on disk.
        if plain_resume.is_none()
            && let Ok(meta) = fs::metadata(&dst)
            && meta.is_file()
            && meta.len() == entry_size
        {
            let mut rem = entry_size;
            while rem > 0 {
                if cancel.load(Ordering::Relaxed) {
                    return Err(StreamUnpackError::Cancelled);
                }
                let want = (rem as usize).min(buf.len());
                let n = entry.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                rem -= n as u64;
            }
            skipped_resumed += 1;
            if let Some(p) = progress {
                p.files_done.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }

        let tmp = PathBuf::from(format!("{}.acpart", dst.display()));
        let mut out = BufWriter::with_capacity(IO_BUF_SIZE, File::create(&tmp)?);
        let mut entry_written = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(out);
                let _ = fs::remove_file(&tmp);
                return Err(StreamUnpackError::Cancelled);
            }
            let n = match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    drop(out);
                    let _ = fs::remove_file(&tmp);
                    return Err(StreamUnpackError::Cancelled);
                }
                Err(e) => {
                    drop(out);
                    let _ = fs::remove_file(&tmp);
                    return Err(StreamUnpackError::Io(e));
                }
            };
            out.write_all(&buf[..n])?;
            entry_written += n as u64;
        }
        out.flush()?;
        drop(out);

        if dst.exists() {
            let _ = fs::remove_file(&dst);
        }
        fs::rename(&tmp, &dst)?;
        bytes += entry_written;
        files += 1;
        if let Some(p) = progress {
            p.files_done.fetch_add(1, Ordering::Relaxed);
        }

        if let Some((resume_path, total_size, base_offset, prior_files, prior_bytes)) = plain_resume
        {
            let data_pos = entry.raw_file_position();
            let padded = entry_size.div_ceil(512).saturating_mul(512);
            let next_rel = data_pos.max(raw_header_pos + 512).saturating_add(padded);
            let next_abs = base_offset.saturating_add(next_rel);
            if next_abs <= total_size {
                let st = TarResumeState {
                    total_size,
                    next_offset: next_abs,
                    files_done: prior_files + files,
                    bytes_written: prior_bytes + bytes,
                };
                if let Ok(text) = serde_json::to_string(&st) {
                    let _ = write_atomic(resume_path, &text);
                }
            }
        }
    }
    Ok((files, bytes, skipped_resumed))
}

/// Stream-download & unpack `.tar`, `.tar.gz`, `.tar.xz`, or `.tar.zst` directly to `dest_dir`
/// without ever writing the compressed archive file to disk. Plain `.tar` supports true HTTP Range
/// pause/resume & automatic mid-stream reconnect at entry boundaries; compressed `.tar.*` supports
/// instant pause, retries, and skip-existing.
pub async fn unpack_tar_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let http = UnpackHttp::single(client.clone());
    unpack_tar_url_with_http(&http, url, dest_dir, cancel, progress).await
}

/// [`unpack_tar_url`] using an engine-configured [`UnpackHttp`] transport.
pub async fn unpack_tar_url_with_http(
    http: &UnpackHttp,
    url: &Url,
    dest_dir: &Path,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    fs::create_dir_all(dest_dir)?;
    let lower = url.path().to_ascii_lowercase();
    let is_plain_tar = lower.ends_with(".tar");
    let resume_path = dest_dir.join(RESUME_FILE_NAME);
    let mut attempt = 0usize;
    let mut total_downloaded = 0u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let saved_tar = if is_plain_tar {
            fs::read_to_string(&resume_path)
                .ok()
                .and_then(|s| serde_json::from_str::<TarResumeState>(&s).ok())
                .filter(|r| r.next_offset > 0 && r.next_offset.is_multiple_of(512))
        } else {
            None
        };

        let (nic, mut req) = http.get(url);
        if let Some(st) = &saved_tar {
            req = req.header(reqwest::header::RANGE, format!("bytes={}-", st.next_offset));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) if attempt < http.max_retries => {
                attempt += 1;
                if let Some(p) = &progress {
                    p.push_event(format!(
                        "Reconnecting tar stream (attempt {attempt}/{})",
                        http.max_retries
                    ));
                }
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Err(e) => return Err(StreamUnpackError::Http(e.to_string())),
        };
        if !resp.status().is_success() {
            if is_retryable_status(resp.status()) && attempt < http.max_retries {
                attempt += 1;
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            return Err(StreamUnpackError::Http(format!("HTTP {}", resp.status())));
        }

        let (base_offset, prior_files, prior_bytes, total_size) = if let Some(st) = saved_tar
            && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
        {
            let total = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.rsplit('/').next())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(st.total_size);
            if total == st.total_size {
                (st.next_offset, st.files_done, st.bytes_written, total)
            } else {
                (0, 0, 0, resp.content_length().unwrap_or(0))
            }
        } else {
            (0, 0, 0, resp.content_length().unwrap_or(0))
        };

        if let Some(p) = &progress {
            if total_size > 0 {
                p.total_bytes.store(total_size, Ordering::Relaxed);
            }
            p.done_bytes.store(base_offset, Ordering::Relaxed);
            p.files_done.store(prior_files, Ordering::Relaxed);
            p.active_workers.store(1, Ordering::Relaxed);
            p.push_event(if base_offset > 0 {
                format!("Resuming tar stream from byte {base_offset} ({prior_files} files done)")
            } else {
                "Streaming tar archive to disk (1× disk)".into()
            });
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, String>>(16);
        let dest_owned = dest_dir.to_owned();
        let cancel_worker = cancel.clone();
        let prog_worker = progress.clone();
        let resume_worker = resume_path.clone();

        let unpack_task = tokio::task::spawn_blocking(move || {
            let mut reader = AsyncChannelReader {
                rx,
                buf: bytes::Bytes::new(),
                pos: 0,
                cancel: cancel_worker.clone(),
            };
            if base_offset > 0 {
                let plain_opt = (total_size > 0).then_some((
                    resume_worker.as_path(),
                    total_size,
                    base_offset,
                    prior_files,
                    prior_bytes,
                ));
                return unpack_tar_reader(
                    reader,
                    &dest_owned,
                    &cancel_worker,
                    prog_worker.as_ref(),
                    plain_opt,
                );
            }
            let mut head = vec![0u8; 6];
            let mut got = 0usize;
            while got < head.len() {
                let n = reader.read(&mut head[got..])?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            head.truncate(got);
            let pref = PrefixedReader {
                head: head.clone(),
                pos: 0,
                inner: reader,
            };
            if head.starts_with(&[0x1f, 0x8b]) {
                let dec = flate2::read::MultiGzDecoder::new(pref);
                unpack_tar_reader(dec, &dest_owned, &cancel_worker, prog_worker.as_ref(), None)
            } else if head.starts_with(b"\xfd7zXZ\x00") {
                let dec = lzma_rust2::XzReader::new(pref, true);
                unpack_tar_reader(dec, &dest_owned, &cancel_worker, prog_worker.as_ref(), None)
            } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                let dec = ruzstd::StreamingDecoder::new(pref).map_err(|e| {
                    StreamUnpackError::BadArchive(format!("zstd header error: {e}"))
                })?;
                unpack_tar_reader(dec, &dest_owned, &cancel_worker, prog_worker.as_ref(), None)
            } else {
                let plain_opt = (is_plain_tar && total_size > 0).then_some((
                    resume_worker.as_path(),
                    total_size,
                    0,
                    0,
                    0,
                ));
                unpack_tar_reader(
                    pref,
                    &dest_owned,
                    &cancel_worker,
                    prog_worker.as_ref(),
                    plain_opt,
                )
            }
        });

        let mut body = resp.bytes_stream();
        let mut downloaded = 0u64;
        let mut transport_err = None;
        while let Some(chunk) = body.next().await {
            if cancel.load(Ordering::Relaxed) {
                drop(tx);
                let _ = unpack_task.await;
                if let Some(p) = &progress {
                    p.active_workers.store(0, Ordering::Relaxed);
                }
                return Err(StreamUnpackError::Cancelled);
            }
            match chunk {
                Ok(b) => {
                    let len = b.len() as u64;
                    downloaded += len;
                    total_downloaded += len;
                    http.record_nic(progress.as_ref(), nic.as_deref(), len);
                    if let Some(p) = &progress {
                        p.done_bytes
                            .store(base_offset + downloaded, Ordering::Relaxed);
                    }
                    pace_async(progress.as_ref(), len, &cancel).await;
                    if tx.send(Ok(b)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    transport_err = Some(e.to_string());
                    let _ = tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
        drop(tx);

        match unpack_task
            .await
            .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))?
        {
            Ok((files, bytes_written, skipped_inline)) => {
                let _ = fs::remove_file(&resume_path);
                if let Some(p) = &progress {
                    p.active_workers.store(0, Ordering::Relaxed);
                    p.files_done
                        .store(prior_files + files + skipped_inline, Ordering::Relaxed);
                    p.files_total
                        .store(prior_files + files + skipped_inline, Ordering::Relaxed);
                }
                return Ok(StreamUnpackReport {
                    files,
                    bytes_written: prior_bytes + bytes_written,
                    bytes_downloaded: total_downloaded,
                    dest_dir: dest_dir.to_owned(),
                    skipped_resumed: prior_files + skipped_inline,
                });
            }
            Err(StreamUnpackError::Cancelled) => {
                if let Some(p) = &progress {
                    p.active_workers.store(0, Ordering::Relaxed);
                }
                return Err(StreamUnpackError::Cancelled);
            }
            Err(e)
                if (transport_err.is_some() || matches!(e, StreamUnpackError::Io(_)))
                    && attempt < http.max_retries
                    && !cancel.load(Ordering::Relaxed) =>
            {
                attempt += 1;
                if let Some(p) = &progress {
                    p.push_event(format!(
                        "Reconnecting tar stream after drop (attempt {attempt}/{})",
                        http.max_retries
                    ));
                }
                http.backoff(attempt, &cancel).await?;
                continue;
            }
            Err(e) => {
                if let Some(p) = &progress {
                    p.active_workers.store(0, Ordering::Relaxed);
                }
                return Err(e);
            }
        }
    }
}

/// Unified streaming unpack entrypoint for any supported URL (`.acpkg`, `.zip`, `.tar.*`).
pub async fn unpack_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    unpack_url_with_progress(client, url, dest_dir, concurrency, cancel, None).await
}

/// [`unpack_url`] with optional live progress counters for UI display.
pub async fn unpack_url_with_progress(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let http = UnpackHttp::single(client.clone());
    unpack_url_with_http(&http, url, dest_dir, concurrency, cancel, progress).await
}

/// Full engine-configured streaming unpack entrypoint: supports Multi-NIC clients, cookies/auth
/// headers, automatic exponential backoff retries, and parallel multi-connection ZIP STORE slicing.
pub async fn unpack_url_with_http(
    http: &UnpackHttp,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    unpack_url_with_http_and_hint(http, url, None, dest_dir, concurrency, cancel, progress).await
}

/// Like [`unpack_url_with_http`], with an optional filename hint (e.g. from browser capture or
/// `Content-Disposition`) so signed CDN URLs without an extension in the path still stream-unpack.
pub async fn unpack_url_with_http_and_hint(
    http: &UnpackHttp,
    url: &Url,
    name_hint: Option<&str>,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let url_lower = url.path().to_ascii_lowercase();
    let url_trimmed = url_lower.trim_end_matches('/');
    let hint_lower = name_hint.map(|s| s.trim().to_ascii_lowercase());
    let candidate = if is_stream_unpack_name(url_trimmed) || url_trimmed.ends_with("/manifest.json")
    {
        url_trimmed
    } else if let Some(ref h) = hint_lower {
        h.trim_end_matches('/')
    } else {
        url_trimmed
    };
    if candidate.ends_with(".acpkg") || candidate.ends_with("/manifest.json") {
        unpack_acpkg_url_with_http(http, url, dest_dir, concurrency, cancel, progress).await
    } else if candidate.ends_with(".zip") {
        unpack_zip_url_with_http(http, url, dest_dir, concurrency, cancel, progress).await
    } else if candidate.ends_with(".tar")
        || candidate.ends_with(".tar.gz")
        || candidate.ends_with(".tgz")
        || candidate.ends_with(".tar.xz")
        || candidate.ends_with(".txz")
        || candidate.ends_with(".tar.zst")
        || candidate.ends_with(".tzst")
    {
        unpack_tar_url_with_http(http, url, dest_dir, cancel, progress).await
    } else {
        Err(StreamUnpackError::Unsupported(format!(
            "cannot stream-unpack {}",
            url.path()
        )))
    }
}
