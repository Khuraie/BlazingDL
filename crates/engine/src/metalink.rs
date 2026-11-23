//! Metalink v4 (RFC 5854 `.meta4`) and v3 (`.metalink`) parser for multi-mirror downloads.

use std::path::Path;
use url::Url;

use crate::verify::Hash;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mirror {
    pub url: Url,
    pub priority: u32,
    pub location: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetalinkFile {
    pub name: String,
    pub size: Option<u64>,
    pub hashes: Vec<Hash>,
    /// Ordered fastest/highest-priority first.
    pub mirrors: Vec<Mirror>,
}

impl MetalinkFile {
    /// Primary download URL (highest priority mirror).
    pub fn primary_url(&self) -> Option<&Url> {
        self.mirrors.first().map(|m| &m.url)
    }

    /// Additional mirror URLs beyond the primary.
    pub fn extra_mirrors(&self) -> Vec<Url> {
        self.mirrors.iter().skip(1).map(|m| m.url.clone()).collect()
    }

    /// Strongest declared checksum hash (SHA-256 > SHA-1 > MD5).
    pub fn best_hash(&self) -> Option<Hash> {
        self.hashes
            .iter()
            .max_by_key(|h| match h.algo {
                crate::verify::Algo::Sha512 => 4,
                crate::verify::Algo::Sha256 => 3,
                crate::verify::Algo::Sha1 => 2,
                crate::verify::Algo::Md5 => 1,
                crate::verify::Algo::Crc32 => 0,
            })
            .cloned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MetalinkError {
    #[error("invalid Metalink XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("not a Metalink document")]
    NotMetalink,
    #[error("Metalink contains no downloadable files with valid HTTP/FTP mirrors")]
    Empty,
}

/// True if `path` ends with `.meta4` or `.metalink` (case-insensitive).
pub fn is_metalink_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("meta4") || e.eq_ignore_ascii_case("metalink"))
}

/// Parse a Metalink v4 (RFC 5854) or v3 XML document into downloadable files & sorted mirrors.
pub fn parse(xml: &str) -> Result<Vec<MetalinkFile>, MetalinkError> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "metalink" {
        return Err(MetalinkError::NotMetalink);
    }
    let is_v3 = root
        .children()
        .any(|n| n.is_element() && n.tag_name().name() == "files");
    let file_nodes: Vec<roxmltree::Node<'_, '_>> = if is_v3 {
        root.children()
            .filter(|n| n.is_element() && n.tag_name().name() == "files")
            .flat_map(|f| f.children())
            .filter(|n| n.is_element() && n.tag_name().name() == "file")
            .collect()
    } else {
        root.children()
            .filter(|n| n.is_element() && n.tag_name().name() == "file")
            .collect()
    };

    let mut out = Vec::new();
    for file in file_nodes {
        let name = file.attribute("name").unwrap_or("").trim().to_owned();
        let mut size = None;
        let mut hashes = Vec::new();
        let mut mirrors = Vec::new();

        for child in file.children().filter(|n| n.is_element()) {
            match child.tag_name().name() {
                "size" => {
                    size = child
                        .text()
                        .and_then(|t| t.trim().parse::<u64>().ok());
                }
                "hash" => {
                    if let (Some(kind), Some(val)) = (child.attribute("type"), child.text())
                        && let Some(h) = parse_hash(kind, val)
                    {
                        hashes.push(h);
                    }
                }
                "verification" => {
                    for hnode in child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "hash")
                    {
                        if let (Some(kind), Some(val)) = (hnode.attribute("type"), hnode.text())
                            && let Some(h) = parse_hash(kind, val)
                        {
                            hashes.push(h);
                        }
                    }
                }
                "url" => {
                    if let Some(m) = parse_url_node(child, is_v3) {
                        mirrors.push(m);
                    }
                }
                "resources" => {
                    for unode in child
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "url")
                    {
                        if let Some(m) = parse_url_node(unode, true) {
                            mirrors.push(m);
                        }
                    }
                }
                _ => {}
            }
        }
        // Lower priority number = preferred in v4 (1 is best); v3 preference was inverted below.
        mirrors.sort_by_key(|m| m.priority);
        if !mirrors.is_empty() {
            let fallback_name = if name.is_empty() {
                mirrors[0]
                    .url
                    .path_segments()
                    .and_then(|mut s| s.next_back())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("download")
                    .to_owned()
            } else {
                name
            };
            out.push(MetalinkFile {
                name: fallback_name,
                size,
                hashes,
                mirrors,
            });
        }
    }
    if out.is_empty() {
        return Err(MetalinkError::Empty);
    }
    Ok(out)
}

fn parse_hash(kind: &str, value: &str) -> Option<Hash> {
    let algo = match kind.trim().to_ascii_lowercase().as_str() {
        "sha-256" | "sha256" => "sha256",
        "sha-1" | "sha1" => "sha1",
        "md5" => "md5",
        "crc32" => "crc32",
        _ => return None,
    };
    Hash::parse(&format!("{algo}:{}", value.trim()))
}

fn parse_url_node(node: roxmltree::Node<'_, '_>, v3: bool) -> Option<Mirror> {
    let raw = node.text()?.trim();
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "ftp" | "ftps") {
        return None;
    }
    let priority = if v3 {
        // v3 uses preference 1..100 where 100 is highest -> invert to lower-is-better.
        let pref = node
            .attribute("preference")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(50);
        1000u32.saturating_sub(pref)
    } else {
        node.attribute("priority")
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(500)
    };
    let location = node.attribute("location").map(|s| s.trim().to_owned());
    Some(Mirror {
        url,
        priority,
        location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metalink_v4_priority_order_and_hashes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <metalink xmlns="urn:ietf:params:xml:ns:metalink">
          <file name="ubuntu-26.04.iso">
            <size>5368709120</size>
            <hash type="md5">d41d8cd98f00b204e9800998ecf8427e</hash>
            <hash type="sha-256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
            <url location="de" priority="20">https://mirror-de.example.org/ubuntu.iso</url>
            <url location="us" priority="5">https://mirror-us.example.org/ubuntu.iso</url>
            <url location="jp" priority="50">https://mirror-jp.example.org/ubuntu.iso</url>
          </file>
        </metalink>"#;
        let files = parse(xml).unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.name, "ubuntu-26.04.iso");
        assert_eq!(f.size, Some(5_368_709_120));
        assert_eq!(f.mirrors.len(), 3);
        assert_eq!(f.primary_url().unwrap().as_str(), "https://mirror-us.example.org/ubuntu.iso");
        assert_eq!(f.extra_mirrors().len(), 2);
        assert_eq!(f.best_hash().unwrap().algo, crate::verify::Algo::Sha256);
    }

    #[test]
    fn parses_metalink_v3_preferences() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <metalink version="3.0" xmlns="http://www.metalinker.org/">
          <files>
            <file name="tool.tar.gz">
              <size>1024</size>
              <verification>
                <hash type="sha256">e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855</hash>
              </verification>
              <resources>
                <url type="http" preference="40">https://slow.example.org/tool.tar.gz</url>
                <url type="http" preference="95">https://fast.example.org/tool.tar.gz</url>
              </resources>
            </file>
          </files>
        </metalink>"#;
        let files = parse(xml).unwrap();
        assert_eq!(files[0].primary_url().unwrap().as_str(), "https://fast.example.org/tool.tar.gz");
        assert_eq!(files[0].size, Some(1024));
    }
}
