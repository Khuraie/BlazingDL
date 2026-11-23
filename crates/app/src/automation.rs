//! Smart Checksum Auto-Match & Post-Download Automation Rules / Webhooks.
//!
//! - [`extract_checksum`]: finds SHA-512 / SHA-256 / SHA-1 / MD5 hashes in pasted text,
//!   clipboard snippets, or GNU/BSD checksum files (`SHA256SUMS`, `.sha256`, etc.).
//! - [`find_sibling_checksum`]: checks next to a downloaded file for sibling checksum files.
//! - [`AutomationRule`] & [`run_rules_async`]: post-download regex/category matching with
//!   auto-move, custom command templates (`{path}`, `{file}`, `{dir}`, `{url}`, `{size}`),
//!   and async webhooks (generic JSON, Discord, and ntfy).

use std::path::{Path, PathBuf};
use std::time::Duration;

use blazingdl_engine::verify::{Algo, Hash};
use serde::{Deserialize, Serialize};

/// A user-configured post-download automation rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationRule {
    pub name: String,
    pub enabled: bool,
    /// Regex matched against the downloaded file's basename (empty matches all files).
    pub pattern: String,
    /// Optional case-insensitive category name filter (empty/None matches any category).
    pub category: Option<String>,
    /// Optional destination folder to move the completed file into.
    pub move_to: Option<String>,
    /// Optional shell command template supporting `{path}`, `{file}`, `{dir}`, `{url}`, `{size}`.
    pub run_command: Option<String>,
    /// Optional webhook URL (auto-detects Discord, ntfy, or posts rich JSON).
    pub webhook_url: Option<String>,
}

impl Default for AutomationRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            pattern: String::new(),
            category: None,
            move_to: None,
            run_command: None,
            webhook_url: None,
        }
    }
}

impl AutomationRule {
    /// Whether this rule applies to a completed download with `file_name` and `category`.
    pub fn matches(&self, file_name: &str, category: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(cat) = self.category.as_deref().map(str::trim).filter(|c| !c.is_empty())
            && !cat.eq_ignore_ascii_case(category.trim())
        {
            return false;
        }
        let pat = self.pattern.trim();
        if pat.is_empty() {
            return true;
        }
        regex::RegexBuilder::new(pat)
            .case_insensitive(true)
            .size_limit(64 * 1024)
            .build()
            .is_ok_and(|re| re.is_match(file_name))
    }
}

/// Snapshot of a completed download passed to post-download automation rules.
#[derive(Clone, Debug)]
pub struct CompletedInfo {
    pub file_name: String,
    pub path: PathBuf,
    pub url: String,
    pub size: u64,
    pub category: String,
}

/// Extract the strongest checksum hash from arbitrary text (pasted URL+hash, GNU `SHA256SUMS`,
/// BSD `SHA256 (file) = ...`, or prefixed `sha256:...`). When `filename` is given and multiple
/// lines appear, lines mentioning that filename take priority.
pub fn extract_checksum(text: &str, filename: Option<&str>) -> Option<Hash> {
    let target = filename
        .and_then(|f| Path::new(f).file_name())
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase);

    let mut matched_lines: Vec<Hash> = Vec::new();
    let mut any_lines: Vec<Hash> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_lower = line.to_ascii_lowercase();
        let line_matches_file = target
            .as_deref()
            .is_some_and(|name| !name.is_empty() && line_lower.contains(name));

        // BSD format: `SHA256 (ubuntu.iso) = <hex>`
        if let Some((lhs, rhs)) = line.split_once('=') {
            let algo_word = lhs.split_whitespace().next().unwrap_or("");
            if let Some(h) = Hash::parse(&format!("{algo_word}:{}", rhs.trim())) {
                if line_matches_file {
                    matched_lines.push(h);
                } else {
                    any_lines.push(h);
                }
                continue;
            }
        }

        // Token scan: prefixed (`sha256:...`, `md5=...`) or bare hex (32/40/64/128 hex chars).
        for raw_tok in line.split_whitespace() {
            let tok = raw_tok.trim_matches(|c: char| matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '[' | ']'));
            if tok.len() < 32 {
                continue;
            }
            if let Some(h) = Hash::parse(tok) {
                // Skip 8-char bare CRC32 in free prose to avoid false positives on random hex words.
                if h.algo == Algo::Crc32 && !tok.contains(':') && !tok.contains('=') {
                    continue;
                }
                if line_matches_file {
                    matched_lines.push(h);
                } else {
                    any_lines.push(h);
                }
            }
        }
    }

    let pool = if !matched_lines.is_empty() {
        matched_lines
    } else if target.is_some() && text.lines().filter(|l| !l.trim().is_empty()).count() > 2 {
        // Multi-line manifest (e.g. SHA256SUMS) where our filename was not listed: do not guess.
        return None;
    } else {
        any_lines
    };

    pool.into_iter().max_by_key(|h| algo_rank(h.algo))
}

fn algo_rank(a: Algo) -> u8 {
    match a {
        Algo::Sha512 => 4,
        Algo::Sha256 => 3,
        Algo::Sha1 => 2,
        Algo::Md5 => 1,
        Algo::Crc32 => 0,
    }
}

/// Check next to `dest` for sibling checksum manifests (`<file>.sha256`, `SHA256SUMS`, etc.).
pub fn find_sibling_checksum(dest: &Path) -> Option<Hash> {
    let dir = dest.parent()?;
    let fname = dest.file_name()?.to_str()?;
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(format!("{}.sha512", dest.display())),
        PathBuf::from(format!("{}.sha256", dest.display())),
        PathBuf::from(format!("{}.sha256sum", dest.display())),
        PathBuf::from(format!("{}.sha1", dest.display())),
        PathBuf::from(format!("{}.md5", dest.display())),
    ];
    for manifest in [
        "SHA512SUMS",
        "SHA256SUMS",
        "SHA256SUMS.txt",
        "sha256sums.txt",
        "SHA1SUMS",
        "MD5SUMS",
    ] {
        candidates.push(dir.join(manifest));
    }
    for cand in candidates {
        let Ok(meta) = std::fs::metadata(&cand) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 256 * 1024 {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&cand)
            && let Some(h) = extract_checksum(&body, Some(fname))
        {
            return Some(h);
        }
    }
    None
}

/// Spawn enabled matching automation rules asynchronously on `rt` without blocking the UI thread.
pub fn run_rules_async(
    rt: &tokio::runtime::Handle,
    rules: &[AutomationRule],
    info: CompletedInfo,
) {
    let matched: Vec<AutomationRule> = rules
        .iter()
        .filter(|r| r.matches(&info.file_name, &info.category))
        .cloned()
        .collect();
    if matched.is_empty() {
        return;
    }
    rt.spawn(async move {
        let _ = execute_rules(&matched, info).await;
    });
}

/// Execute a list of already-matched rules sequentially, returning the final file path.
pub async fn execute_rules(rules: &[AutomationRule], mut info: CompletedInfo) -> PathBuf {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .ok();

    for rule in rules {
        // 1. Optional auto-move
        if let Some(dest_dir) = rule.move_to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let target_dir = PathBuf::from(dest_dir);
            if let Ok(moved) = move_file_into(&info.path, &target_dir) {
                info.path = moved;
                if let Some(name) = info.path.file_name().and_then(|n| n.to_str()) {
                    info.file_name = name.to_owned();
                }
            }
        }

        // 2. Optional custom command template
        if let Some(cmd_tpl) = rule
            .run_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let expanded = expand_template(cmd_tpl, &info);
            spawn_shell_command(&expanded);
        }

        // 3. Optional webhook notification
        if let Some(url) = rule
            .webhook_url
            .as_deref()
            .map(str::trim)
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            && let Some(c) = &client
        {
            let _ = post_webhook(c, url, &info).await;
        }
    }
    info.path
}

fn move_file_into(src: &Path, target_dir: &Path) -> std::io::Result<PathBuf> {
    if !src.exists() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    std::fs::create_dir_all(target_dir)?;
    let name = src
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let dst = target_dir.join(name);
    if dst == src {
        return Ok(dst);
    }
    match std::fs::rename(src, &dst) {
        Ok(()) => Ok(dst),
        // Cross-volume move fallback: copy then remove original.
        Err(_) => {
            std::fs::copy(src, &dst)?;
            std::fs::remove_file(src)?;
            Ok(dst)
        }
    }
}

pub fn expand_template(tpl: &str, info: &CompletedInfo) -> String {
    let dir = info
        .path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    tpl.replace("{path}", &info.path.to_string_lossy())
        .replace("{file}", &info.file_name)
        .replace("{dir}", &dir)
        .replace("{url}", &info.url)
        .replace("{size}", &info.size.to_string())
        .replace("{category}", &info.category)
}

fn spawn_shell_command(command: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/C", command])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("sh")
            .args(["-c", command])
            .spawn();
    }
}

async fn post_webhook(
    client: &reqwest::Client,
    url: &str,
    info: &CompletedInfo,
) -> Result<(), reqwest::Error> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("discord.com/api/webhooks") || lower.contains("discordapp.com/api/webhooks") {
        let payload = serde_json::json!({
            "content": format!(
                "**BlazingDL Complete:** `{}` ({} bytes)\nSaved to `{}`",
                info.file_name,
                info.size,
                info.path.display()
            )
        });
        client.post(url).json(&payload).send().await?;
    } else if lower.contains("ntfy.sh/") {
        client
            .post(url)
            .header("Title", format!("BlazingDL: {}", info.file_name))
            .body(format!(
                "Downloaded {} ({} bytes) -> {}",
                info.file_name,
                info.size,
                info.path.display()
            ))
            .send()
            .await?;
    } else {
        let payload = serde_json::json!({
            "event": "download.complete",
            "file": info.file_name,
            "path": info.path.to_string_lossy(),
            "url": info.url,
            "size": info.size,
            "category": info.category,
        });
        client.post(url).json(&payload).send().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn extracts_gnu_bsd_prefixed_and_bare_checksums() {
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";

        // GNU SHA256SUMS multi-line manifest picks the matching filename.
        let manifest = format!(
            "{sha1}  other.iso\n\
             {sha256} *ubuntu-24.04.iso\n\
             0123456789abcdef0123456789abcdef  third.bin\n"
        );
        let h = extract_checksum(&manifest, Some("ubuntu-24.04.iso")).unwrap();
        assert_eq!(h.algo, Algo::Sha256);
        assert_eq!(h.display(), format!("sha256:{sha256}"));

        // Unlisted file in multi-line manifest returns None instead of wrong hash.
        assert!(extract_checksum(&manifest, Some("missing.zip")).is_none());

        // BSD format.
        let bsd = format!("SHA256 (release.tar.gz) = {sha256}");
        let h2 = extract_checksum(&bsd, Some("release.tar.gz")).unwrap();
        assert_eq!(h2.algo, Algo::Sha256);

        // Prefixed pasted text alongside URL.
        let pasted = format!("https://example.com/a.bin sha256:{sha256}");
        let h3 = extract_checksum(&pasted, Some("a.bin")).unwrap();
        assert_eq!(h3.algo, Algo::Sha256);
    }

    #[test]
    fn matches_sibling_sha256sums_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("archive.tar.zst");
        std::fs::write(&file, b"payload").unwrap();
        let sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        std::fs::write(
            dir.path().join("SHA256SUMS"),
            format!("{sha}  archive.tar.zst\n"),
        )
        .unwrap();
        let found = find_sibling_checksum(&file).expect("sibling hash found");
        assert_eq!(found.algo, Algo::Sha256);
        assert_eq!(found.display(), format!("sha256:{sha}"));
    }

    #[test]
    fn rule_matching_and_template_expansion() {
        let rule = AutomationRule {
            name: "ISOs".into(),
            enabled: true,
            pattern: r"\.iso$".into(),
            category: Some("Programs".into()),
            ..Default::default()
        };
        assert!(rule.matches("Fedora-42.ISO", "programs"));
        assert!(!rule.matches("Fedora-42.zip", "programs"));
        assert!(!rule.matches("Fedora-42.iso", "Video"));

        let info = CompletedInfo {
            file_name: "app.iso".into(),
            path: PathBuf::from("C:/Downloads/app.iso"),
            url: "https://example.com/app.iso".into(),
            size: 1024,
            category: "Programs".into(),
        };
        assert_eq!(
            expand_template("verify {file} {size} {category}", &info),
            "verify app.iso 1024 Programs"
        );
    }

    #[tokio::test]
    async fn rule_moves_file_and_posts_webhook() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src_file = src_dir.path().join("game-patch.zip");
        std::fs::write(&src_file, b"blazing-payload").unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .await;
            req
        });

        let rule = AutomationRule {
            name: "Move and Notify".into(),
            enabled: true,
            pattern: r"\.zip$".into(),
            move_to: Some(dst_dir.path().to_string_lossy().into_owned()),
            webhook_url: Some(format!("http://{addr}/hook")),
            ..Default::default()
        };
        let info = CompletedInfo {
            file_name: "game-patch.zip".into(),
            path: src_file.clone(),
            url: "https://cdn.example.com/game-patch.zip".into(),
            size: 15,
            category: "Compressed".into(),
        };

        let final_path = execute_rules(&[rule], info).await;
        let expected_dst = dst_dir.path().join("game-patch.zip");
        assert_eq!(final_path, expected_dst);
        assert!(!src_file.exists(), "original file moved");
        assert_eq!(std::fs::read(&expected_dst).unwrap(), b"blazing-payload");

        let received = server.await.unwrap();
        assert!(received.contains("POST /hook HTTP/1.1"));
        assert!(received.contains("\"event\":\"download.complete\""));
        assert!(received.contains("game-patch.zip"));
    }
}
