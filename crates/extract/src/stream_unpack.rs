//! Steam-style 1× disk space streaming download + decompression (inspired by and wire-compatible
//! with `Khuraie/AntiCompress`).
//!
//! Supported streaming targets (the archive never sits on disk alongside its unpacked files):
//! - **Remote `.zip`** via HTTP `Range` Central Directory + parallel entry decompression +
//!   CRC32 verification + resumable `.anticompress-resume.json` state.
//! - **Remote `.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`, `.tar.zst`/`.tzst`** piped directly
//!   from the HTTP body through decompression and tar extraction straight to the output folder.
//! - **AntiCompress `.acpkg` v1** (local folder or remote HTTP/HTTPS): canonical SHA-256 manifest
//!   verification, parallel `chunk-NNNNNN.zst` fetch + per-chunk SHA-256 check, pure-Rust `ruzstd`
//!   decompression, per-file SHA-256 verification, atomic `.acpart` renames, and free resume.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamUnpackReport {
    pub files: u64,
    pub bytes_written: u64,
    pub bytes_downloaded: u64,
    pub dest_dir: PathBuf,
    pub skipped_resumed: u64,
}

/// Live progress counters shared between a streaming unpack task and the UI transfer row.
#[derive(Default, Debug)]
pub struct UnpackProgress {
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
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

/// True when `url` points to a stream-unpackable archive (`.zip`, `.tar.*`, or `.acpkg`).
pub fn is_stream_unpack_url(url: &Url) -> bool {
    let lower = url.path().to_ascii_lowercase();
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
        || trimmed.ends_with("/manifest.json")
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
                .saturating_sub(index as u64 * self.chunk_size)
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

fn verify_existing_file(path: &Path, expected_size: u64, expected_sha256: &str) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() != expected_size {
        return false;
    }
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_sha256)
}

fn decompress_zstd_chunk(compressed: &[u8]) -> Result<Vec<u8>, StreamUnpackError> {
    let mut dec = ruzstd::StreamingDecoder::new(compressed)
        .map_err(|e| StreamUnpackError::BadArchive(format!("zstd header error: {e}")))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| StreamUnpackError::BadArchive(format!("zstd decode error: {e}")))?;
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
    active_tmp: Option<(PathBuf, PathBuf, File, Sha256)>,
    files_done: u64,
    bytes_written: u64,
    skipped_resumed: u64,
}

impl AcAssembler {
    fn new(dest_dir: &Path, files: Vec<AcFileEntry>) -> Result<Self, StreamUnpackError> {
        fs::create_dir_all(dest_dir)?;
        let mut skip_mask = vec![false; files.len()];
        let mut skipped_resumed = 0u64;
        for (i, entry) in files.iter().enumerate() {
            let rel = safe::rel_path(&entry.path, 64).map_err(|why| {
                StreamUnpackError::BadArchive(format!("unsafe path {}: {why}", entry.path))
            })?;
            let final_path = dest_dir.join(&rel);
            if verify_existing_file(&final_path, entry.size, &entry.sha256) {
                skip_mask[i] = true;
                skipped_resumed += 1;
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
            skipped_resumed,
        })
    }

    fn needed_chunk_indices(&self, chunk_size: u64, num_chunks: usize) -> HashSet<usize> {
        let mut needed = HashSet::new();
        for (i, f) in self.files.iter().enumerate() {
            if self.skip_mask[i] || f.size == 0 {
                continue;
            }
            let first = (f.offset / chunk_size) as usize;
            let last = ((f.offset + f.size - 1) / chunk_size) as usize;
            for c in first..=last.min(num_chunks.saturating_sub(1)) {
                needed.insert(c);
            }
        }
        needed
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
        let chunk_end = chunk_offset + data.len() as u64;

        while self.file_idx < self.files.len() {
            let entry = self.files[self.file_idx].clone();
            let file_end = entry.offset + entry.size;
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
                let f = File::create(&tmp)?;
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
            dest_dir: self.dest_dir,
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
    let mut asm = AcAssembler::new(dest_dir, manifest.files.clone())?;
    if let Some(p) = progress {
        p.total_bytes.store(manifest.total_size, Ordering::Relaxed);
        p.files_total
            .store(manifest.files.len() as u64, Ordering::Relaxed);
        p.files_done.store(asm.skipped_resumed, Ordering::Relaxed);
    }
    let needed = asm.needed_chunk_indices(manifest.chunk_size, manifest.chunks.len());
    asm.finish_zero_byte_files()?;

    let mut read_bytes = 0u64;
    for chunk in &manifest.chunks {
        if cancel.load(Ordering::Relaxed) {
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
        let raw = decompress_zstd_chunk(&compressed)?;
        let expected_len = manifest.chunk_expected_size(chunk.index);
        if raw.len() as u64 != expected_len {
            return Err(StreamUnpackError::Integrity(format!(
                "chunk {} size mismatch: expected {expected_len}, got {}",
                chunk.index,
                raw.len()
            )));
        }
        asm.feed_chunk(chunk.index as u64 * manifest.chunk_size, &raw)?;
        if let Some(p) = progress {
            p.done_bytes.store(asm.bytes_written, Ordering::Relaxed);
            p.files_done
                .store(asm.files_done + asm.skipped_resumed, Ordering::Relaxed);
        }
        if delete_chunks {
            let _ = fs::remove_file(&cpath);
        }
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

    let resp = client
        .get(manifest_url)
        .send()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(StreamUnpackError::Http(format!(
            "manifest HTTP {}",
            resp.status()
        )));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    let manifest = parse_acpkg_manifest(&text)?;
    let mut asm = AcAssembler::new(dest_dir, manifest.files.clone())?;
    if let Some(p) = &progress {
        p.total_bytes.store(manifest.total_size, Ordering::Relaxed);
        p.files_total
            .store(manifest.files.len() as u64, Ordering::Relaxed);
        p.files_done.store(asm.skipped_resumed, Ordering::Relaxed);
    }
    let needed = asm.needed_chunk_indices(manifest.chunk_size, manifest.chunks.len());
    asm.finish_zero_byte_files()?;

    let to_fetch: Vec<AcChunkInfo> = manifest
        .chunks
        .iter()
        .filter(|c| needed.contains(&c.index))
        .cloned()
        .collect();

    let downloaded = Arc::new(AtomicU64::new(0));
    let window = concurrency.clamp(1, 8);
    let mut stream = futures::stream::iter(to_fetch.into_iter().map(|chunk| {
        let client = client.clone();
        let base_url = base_url.clone();
        let downloaded = downloaded.clone();
        let cancel = cancel.clone();
        let expected_len = manifest.chunk_expected_size(chunk.index);
        async move {
            if cancel.load(Ordering::Relaxed) {
                return Err(StreamUnpackError::Cancelled);
            }
            let cname = AcManifest::chunk_name(chunk.index);
            let curl = base_url
                .join(&cname)
                .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
            let resp = client
                .get(curl)
                .send()
                .await
                .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(StreamUnpackError::Http(format!(
                    "{cname} HTTP {}",
                    resp.status()
                )));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
            downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            let actual_sha = hex::encode(Sha256::digest(&bytes));
            if !actual_sha.eq_ignore_ascii_case(&chunk.sha256) {
                return Err(StreamUnpackError::Integrity(format!(
                    "{cname} SHA-256 mismatch"
                )));
            }
            let raw = tokio::task::spawn_blocking(move || decompress_zstd_chunk(&bytes))
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
        let (idx, raw) = res?;
        asm.feed_chunk(idx as u64 * manifest.chunk_size, &raw)?;
        if let Some(p) = &progress {
            p.done_bytes.store(asm.bytes_written, Ordering::Relaxed);
            p.files_done
                .store(asm.files_done + asm.skipped_resumed, Ordering::Relaxed);
        }
    }

    Ok(asm.report(downloaded.load(Ordering::Relaxed)))
}

// ============================================================================
// 2. Streaming HTTP `.zip` Unpack (Range Central Directory + Parallel Entries + Resume)
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

#[derive(Serialize, Deserialize, Default)]
struct ZipResumeState {
    total_size: u64,
    completed: HashSet<String>,
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

/// Locate Central Directory `(cd_offset, cd_size)` from a ZIP file's tail buffer (supports ZIP64).
fn find_cd_bounds(tail: &[u8], tail_start: u64) -> Option<(u64, u64)> {
    // Search backwards for End of Central Directory signature `PK\x05\x06`.
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
    let mut cd_size = u64::from(read_u32_le(tail, pos + 12));
    let mut cd_offset = u64::from(read_u32_le(tail, pos + 16));

    // Check ZIP64 EOCD Locator (`PK\x06\x07`) 20 bytes before EOCD if sentinel values appear.
    if (cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF)
        && pos >= 20
        && &tail[pos - 20..pos - 16] == b"PK\x06\x07"
    {
        let z64_off = read_u64_le(tail, pos - 12);
        if z64_off >= tail_start {
            let rel = (z64_off - tail_start) as usize;
            if rel + 56 <= tail.len() && &tail[rel..rel + 4] == b"PK\x06\x06" {
                cd_size = read_u64_le(tail, rel + 40);
                cd_offset = read_u64_le(tail, rel + 48);
            }
        }
    }
    Some((cd_offset, cd_size))
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

        let next = i + 46 + name_len + extra_len + comment_len;
        if next > cd.len() {
            return Err(StreamUnpackError::BadArchive(
                "truncated ZIP central directory".into(),
            ));
        }
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + name_len]).into_owned();
        let extra = &cd[i + 46 + name_len..i + 46 + name_len + extra_len];

        // Parse ZIP64 extra field (header ID 0x0001) when any 32-bit field is saturated.
        if uncomp_size == 0xFFFF_FFFF || comp_size == 0xFFFF_FFFF || local_offset == 0xFFFF_FFFF {
            let mut ex = 0usize;
            while ex + 4 <= extra.len() {
                let tag = read_u16_le(extra, ex);
                let sz = read_u16_le(extra, ex + 2) as usize;
                if ex + 4 + sz > extra.len() {
                    break;
                }
                if tag == 0x0001 {
                    let mut p = ex + 4;
                    if uncomp_size == 0xFFFF_FFFF && p + 8 <= ex + 4 + sz {
                        uncomp_size = read_u64_le(extra, p);
                        p += 8;
                    }
                    if comp_size == 0xFFFF_FFFF && p + 8 <= ex + 4 + sz {
                        comp_size = read_u64_le(extra, p);
                        p += 8;
                    }
                    if local_offset == 0xFFFF_FFFF && p + 8 <= ex + 4 + sz {
                        local_offset = read_u64_le(extra, p);
                    }
                    break;
                }
                ex += 4 + sz;
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

async fn fetch_range(
    client: &reqwest::Client,
    url: &Url,
    start: u64,
    end_inclusive: u64,
) -> Result<Vec<u8>, StreamUnpackError> {
    let resp = client
        .get(url.clone())
        .header(
            reqwest::header::RANGE,
            format!("bytes={start}-{end_inclusive}"),
        )
        .send()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status().is_success() {
        return Err(StreamUnpackError::Http(format!(
            "range HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    Ok(bytes.to_vec())
}

fn unpack_zip_entry_payload(
    entry: &ZipCdEntry,
    payload: &[u8],
    dest_dir: &Path,
) -> Result<u64, StreamUnpackError> {
    let rel = safe::rel_path(&entry.name, 64).map_err(|w| {
        StreamUnpackError::BadArchive(format!("unsafe ZIP path {}: {w}", entry.name))
    })?;
    let dst = dest_dir.join(rel);
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = PathBuf::from(format!("{}.acpart", dst.display()));
    let mut out = File::create(&tmp)?;
    let mut crc = crc32fast::Hasher::new();
    let mut written = 0u64;

    let copy_and_hash = |reader: &mut dyn Read,
                         out: &mut File,
                         crc: &mut crc32fast::Hasher,
                         written: &mut u64|
     -> io::Result<()> {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            crc.update(&buf[..n]);
            *written += n as u64;
        }
        Ok(())
    };

    let res = match entry.method {
        0 => {
            let mut cur = io::Cursor::new(payload);
            copy_and_hash(&mut cur, &mut out, &mut crc, &mut written)
        }
        8 => {
            let mut dec = flate2::read::DeflateDecoder::new(payload);
            copy_and_hash(&mut dec, &mut out, &mut crc, &mut written)
        }
        other => {
            let _ = fs::remove_file(&tmp);
            return Err(StreamUnpackError::Unsupported(format!(
                "ZIP compression method {other} for {}",
                entry.name
            )));
        }
    };
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(StreamUnpackError::Io(e));
    }
    out.flush()?;
    drop(out);

    let got_crc = crc.finalize();
    if got_crc != entry.crc32 {
        let _ = fs::remove_file(&tmp);
        return Err(StreamUnpackError::Integrity(format!(
            "CRC32 mismatch on {}: expected {:08x}, got {:08x}",
            entry.name, entry.crc32, got_crc
        )));
    }
    if dst.exists() {
        let _ = fs::remove_file(&dst);
    }
    fs::rename(&tmp, &dst)?;
    Ok(written)
}

/// Stream-unpack a remote `.zip` over HTTP `Range` in parallel (1× disk space, CRC32-verified,
/// pause/resume via `.anticompress-resume.json`).
pub async fn unpack_zip_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    concurrency: usize,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    fs::create_dir_all(dest_dir)?;

    // Discover total file size via HEAD (with bytes=0-0 fallback).
    let head = client
        .head(url.clone())
        .send()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    let total_size = if let Some(len) = head
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 22)
    {
        len
    } else {
        let probe = client
            .get(url.clone())
            .header(reqwest::header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
        probe
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                StreamUnpackError::Unsupported(
                    "server did not announce ZIP size / Range support".into(),
                )
            })?
    };

    // Fetch tail (up to 64 KiB) to locate the Central Directory without downloading the archive.
    let tail_len = total_size.min(65_536);
    let tail_start = total_size - tail_len;
    let tail = fetch_range(client, url, tail_start, total_size - 1).await?;
    let downloaded = Arc::new(AtomicU64::new(tail.len() as u64));

    let (cd_offset, cd_size) = find_cd_bounds(&tail, tail_start)
        .ok_or_else(|| StreamUnpackError::BadArchive("ZIP EOCD not found in tail".into()))?;
    if cd_size == 0 || cd_offset + cd_size > total_size {
        return Err(StreamUnpackError::BadArchive(
            "invalid ZIP central directory bounds".into(),
        ));
    }

    let cd_bytes = if cd_offset >= tail_start {
        let rel = (cd_offset - tail_start) as usize;
        tail[rel..rel + cd_size as usize].to_vec()
    } else {
        let bytes = fetch_range(client, url, cd_offset, cd_offset + cd_size - 1).await?;
        downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        bytes
    };

    let entries = parse_central_directory(&cd_bytes)?;
    let total_uncomp: u64 = entries.iter().map(|e| e.uncomp_size).sum();
    let total_files = entries.len() as u64;
    let resume_path = dest_dir.join(RESUME_FILE_NAME);
    let mut resume = fs::read_to_string(&resume_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ZipResumeState>(&s).ok())
        .filter(|r| r.total_size == total_size)
        .unwrap_or(ZipResumeState {
            total_size,
            completed: HashSet::new(),
        });

    let mut pending = Vec::new();
    let mut skipped_resumed = 0u64;
    let mut already_bytes = 0u64;
    for entry in entries {
        if resume.completed.contains(&entry.name) {
            let ok = safe::rel_path(&entry.name, 64)
                .ok()
                .and_then(|r| fs::metadata(dest_dir.join(r)).ok())
                .is_some_and(|m| m.len() == entry.uncomp_size);
            if ok {
                skipped_resumed += 1;
                already_bytes += entry.uncomp_size;
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
    }

    let resume_state = Arc::new(Mutex::new(resume));
    let bytes_written = Arc::new(AtomicU64::new(0));
    let files_done = Arc::new(AtomicU64::new(0));
    let window = concurrency.clamp(1, 6);

    let mut stream = futures::stream::iter(pending.into_iter().map(|entry| {
        let client = client.clone();
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
            if cancel.load(Ordering::Relaxed) {
                return Err(StreamUnpackError::Cancelled);
            }
            // Fetch local header + compressed payload in a single ranged read (with 256B extra
            // headroom so typical local headers never require a second round-trip).
            let guess_end =
                (entry.local_offset + 30 + entry.name.len() as u64 + 256 + entry.comp_size)
                    .min(total_size - 1);
            let mut buf = fetch_range(&client, &url, entry.local_offset, guess_end).await?;
            downloaded.fetch_add(buf.len() as u64, Ordering::Relaxed);
            if buf.len() < 30 || &buf[0..4] != b"PK\x03\x04" {
                return Err(StreamUnpackError::BadArchive(format!(
                    "missing ZIP local header for {}",
                    entry.name
                )));
            }
            let l_nlen = read_u16_le(&buf, 26) as usize;
            let l_elen = read_u16_le(&buf, 28) as usize;
            let data_start = 30 + l_nlen + l_elen;
            let need_total = data_start + entry.comp_size as usize;
            if buf.len() < need_total {
                let extra_from = entry.local_offset + buf.len() as u64;
                let extra_to = entry.local_offset + need_total as u64 - 1;
                let more = fetch_range(&client, &url, extra_from, extra_to).await?;
                downloaded.fetch_add(more.len() as u64, Ordering::Relaxed);
                buf.extend_from_slice(&more);
            }
            let payload = buf[data_start..need_total].to_vec();
            let e_clone = entry.clone();
            let d_clone = dest_dir.clone();
            let wrote = tokio::task::spawn_blocking(move || {
                unpack_zip_entry_payload(&e_clone, &payload, &d_clone)
            })
            .await
            .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;
            bytes_written.fetch_add(wrote, Ordering::Relaxed);
            files_done.fetch_add(1, Ordering::Relaxed);
            if let Some(p) = &progress {
                p.done_bytes.fetch_add(wrote, Ordering::Relaxed);
                p.files_done.fetch_add(1, Ordering::Relaxed);
            }

            if let Ok(mut guard) = resume_state.lock() {
                guard.completed.insert(entry.name);
                if let Ok(text) = serde_json::to_string(&*guard) {
                    let _ = fs::write(&resume_path, text);
                }
            }
            Ok::<(), StreamUnpackError>(())
        }
    }))
    .buffer_unordered(window);

    while let Some(res) = stream.next().await {
        res?;
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

struct ChannelReader {
    rx: mpsc::Receiver<Result<Vec<u8>, String>>,
    buf: Vec<u8>,
    pos: usize,
    cancel: Arc<AtomicBool>,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) if chunk.is_empty() => continue,
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(io::Error::other(e)),
                Err(_) => return Ok(0),
            }
        }
        let avail = &self.buf[self.pos..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
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

fn unpack_tar_reader<R: Read>(
    reader: R,
    dest_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(u64, u64), StreamUnpackError> {
    fs::create_dir_all(dest_dir)?;
    let mut archive = tar::Archive::new(reader);
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in archive.entries()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(StreamUnpackError::Cancelled);
        }
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
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
        let mut out = File::create(&dst)?;
        bytes += io::copy(&mut entry, &mut out)?;
        files += 1;
    }
    Ok((files, bytes))
}

/// Stream-download & unpack `.tar`, `.tar.gz`, `.tar.xz`, or `.tar.zst` directly to `dest_dir`
/// without ever writing the compressed archive file to disk.
pub async fn unpack_tar_url(
    client: &reqwest::Client,
    url: &Url,
    dest_dir: &Path,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<UnpackProgress>>,
) -> Result<StreamUnpackReport, StreamUnpackError> {
    let resp = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| StreamUnpackError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(StreamUnpackError::Http(format!("HTTP {}", resp.status())));
    }
    if let (Some(p), Some(len)) = (&progress, resp.content_length()) {
        p.total_bytes.store(len, Ordering::Relaxed);
    }

    let (tx, rx) = mpsc::sync_channel::<Result<Vec<u8>, String>>(16);
    let dest_owned = dest_dir.to_owned();
    let cancel_worker = cancel.clone();
    let unpack_task = tokio::task::spawn_blocking(move || {
        let mut reader = ChannelReader {
            rx,
            buf: Vec::new(),
            pos: 0,
            cancel: cancel_worker.clone(),
        };
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
            let dec = flate2::read::GzDecoder::new(pref);
            unpack_tar_reader(dec, &dest_owned, &cancel_worker)
        } else if head.starts_with(b"\xfd7zXZ\x00") {
            let dec = lzma_rust2::XzReader::new(pref, true);
            unpack_tar_reader(dec, &dest_owned, &cancel_worker)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            let dec = ruzstd::StreamingDecoder::new(pref)
                .map_err(|e| StreamUnpackError::BadArchive(format!("zstd header error: {e}")))?;
            unpack_tar_reader(dec, &dest_owned, &cancel_worker)
        } else {
            unpack_tar_reader(pref, &dest_owned, &cancel_worker)
        }
    });

    let mut body = resp.bytes_stream();
    let mut downloaded = 0u64;
    while let Some(chunk) = body.next().await {
        if cancel.load(Ordering::Relaxed) {
            drop(tx);
            let _ = unpack_task.await;
            return Err(StreamUnpackError::Cancelled);
        }
        match chunk {
            Ok(b) => {
                downloaded += b.len() as u64;
                if let Some(p) = &progress {
                    p.done_bytes.store(downloaded, Ordering::Relaxed);
                }
                if tx.send(Ok(b.to_vec())).is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                break;
            }
        }
    }
    drop(tx);

    let (files, bytes_written) = unpack_task
        .await
        .map_err(|e| StreamUnpackError::BadArchive(e.to_string()))??;
    if let Some(p) = &progress {
        p.files_done.store(files, Ordering::Relaxed);
        p.files_total.store(files, Ordering::Relaxed);
    }
    Ok(StreamUnpackReport {
        files,
        bytes_written,
        bytes_downloaded: downloaded,
        dest_dir: dest_dir.to_owned(),
        skipped_resumed: 0,
    })
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
    let lower = url.path().to_ascii_lowercase();
    let trimmed = lower.trim_end_matches('/');
    if trimmed.ends_with(".acpkg") || trimmed.ends_with("/manifest.json") {
        unpack_acpkg_url(client, url, dest_dir, concurrency, cancel, progress).await
    } else if trimmed.ends_with(".zip") {
        unpack_zip_url(client, url, dest_dir, concurrency, cancel, progress).await
    } else if trimmed.ends_with(".tar")
        || trimmed.ends_with(".tar.gz")
        || trimmed.ends_with(".tgz")
        || trimmed.ends_with(".tar.xz")
        || trimmed.ends_with(".txz")
        || trimmed.ends_with(".tar.zst")
        || trimmed.ends_with(".tzst")
    {
        unpack_tar_url(client, url, dest_dir, cancel, progress).await
    } else {
        Err(StreamUnpackError::Unsupported(format!(
            "cannot stream-unpack {}",
            url.path()
        )))
    }
}
