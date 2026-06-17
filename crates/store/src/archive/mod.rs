use std::collections::BTreeMap;
use std::io::{Read, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub mod client;
pub mod worker;

pub use client::{ClientError, PayloadArchiveClient, S3ArchiveClient};
pub use worker::{spawn, PayloadArchiveHandle, PayloadArchiveWorkerConfig};

/// Manifest describing archived payload objects stored for a request.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PayloadArchiveManifest {
    pub request_id: String,
    pub objects: BTreeMap<String, ArchiveObject>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ArchiveObject {
    pub key: String,
    pub original_size: usize,
    pub compressed_size: usize,
    pub sha256_hex: String,
    pub content_type: String,
    pub content_encoding: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveObjectKind {
    CgReqRaw,
    CgReqParsed,
    GpReqRaw,
    GpReqParsed,
    PgRspRaw,
    PgRspParsed,
    GcRspRaw,
    GcRspParsed,
}

impl ArchiveObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CgReqRaw => "cg_req_raw",
            Self::CgReqParsed => "cg_req_parsed",
            Self::GpReqRaw => "gp_req_raw",
            Self::GpReqParsed => "gp_req_parsed",
            Self::PgRspRaw => "pg_rsp_raw",
            Self::PgRspParsed => "pg_rsp_parsed",
            Self::GcRspRaw => "gc_rsp_raw",
            Self::GcRspParsed => "gc_rsp_parsed",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::CgReqRaw
            | Self::CgReqParsed
            | Self::GpReqParsed
            | Self::PgRspParsed
            | Self::GcRspParsed => "json",
            Self::GpReqRaw | Self::PgRspRaw | Self::GcRspRaw => "txt",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::CgReqRaw
            | Self::CgReqParsed
            | Self::GpReqParsed
            | Self::PgRspParsed
            | Self::GcRspParsed => "application/json; charset=utf-8",
            Self::GpReqRaw | Self::PgRspRaw | Self::GcRspRaw => "text/plain; charset=utf-8",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveStatus {
    Pending,
    ArchiveReady,
    Uploading,
    Uploaded,
    Failed,
    Expired,
    Disabled,
}

impl ArchiveStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ArchiveReady => "archive_ready",
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("gzip error: {0}")]
    Gzip(#[from] std::io::Error),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("signing error: {0}")]
    Signing(String),
    #[error("invalid s3 url: {0}")]
    InvalidUrl(String),
}

pub fn normalize_prefix(prefix: &str) -> String {
    prefix
        .split('/')
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn object_key(prefix: &str, request_id: &str, kind: ArchiveObjectKind) -> String {
    let mut parts = Vec::new();
    let prefix = normalize_prefix(prefix);
    if !prefix.is_empty() {
        parts.push(prefix);
    }
    parts.push(request_id.trim_matches('/').to_string());
    parts.push(format!("{}.{}", kind.as_str(), kind.extension()));
    parts.join("/")
}

pub fn gzip_compress(data: &[u8]) -> Result<Vec<u8>, ArchiveError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, ArchiveError> {
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

pub fn build_object_meta(
    kind: ArchiveObjectKind,
    original: &[u8],
    compressed: &[u8],
    key: String,
) -> ArchiveObject {
    ArchiveObject {
        key,
        original_size: original.len(),
        compressed_size: compressed.len(),
        sha256_hex: sha256_hex(original),
        content_type: kind.content_type().to_string(),
        content_encoding: "gzip".to_string(),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ArchivePayload {
    pub request_id: String,
    pub cg_req_raw: Option<String>,
    pub cg_req_parsed: Option<String>,
    pub gp_req_raw: Option<String>,
    pub gp_req_parsed: Option<String>,
    pub pg_rsp_raw: Option<String>,
    pub pg_rsp_parsed: Option<String>,
    pub gc_rsp_raw: Option<String>,
    pub gc_rsp_parsed: Option<String>,
}

impl ArchivePayload {
    pub fn iter(&self, prefix: &str) -> Vec<(ArchiveObjectKind, String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut push = |kind: ArchiveObjectKind, value: &Option<String>| {
            if let Some(v) = value.as_ref() {
                let key = object_key(prefix, &self.request_id, kind);
                out.push((kind, key, v.as_bytes().to_vec()));
            }
        };
        push(ArchiveObjectKind::CgReqRaw, &self.cg_req_raw);
        push(ArchiveObjectKind::CgReqParsed, &self.cg_req_parsed);
        push(ArchiveObjectKind::GpReqRaw, &self.gp_req_raw);
        push(ArchiveObjectKind::GpReqParsed, &self.gp_req_parsed);
        push(ArchiveObjectKind::PgRspRaw, &self.pg_rsp_raw);
        push(ArchiveObjectKind::PgRspParsed, &self.pg_rsp_parsed);
        push(ArchiveObjectKind::GcRspRaw, &self.gc_rsp_raw);
        push(ArchiveObjectKind::GcRspParsed, &self.gc_rsp_parsed);
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn key_and_prefix_are_normalized() {
        assert_eq!(normalize_prefix("//foo//bar/"), "foo/bar");
        let cases = [
            (ArchiveObjectKind::CgReqRaw, "foo/bar/req-1/cg_req_raw.json"),
            (
                ArchiveObjectKind::CgReqParsed,
                "foo/bar/req-1/cg_req_parsed.json",
            ),
            (ArchiveObjectKind::GpReqRaw, "foo/bar/req-1/gp_req_raw.txt"),
            (
                ArchiveObjectKind::GpReqParsed,
                "foo/bar/req-1/gp_req_parsed.json",
            ),
            (ArchiveObjectKind::PgRspRaw, "foo/bar/req-1/pg_rsp_raw.txt"),
            (
                ArchiveObjectKind::PgRspParsed,
                "foo/bar/req-1/pg_rsp_parsed.json",
            ),
            (ArchiveObjectKind::GcRspRaw, "foo/bar/req-1/gc_rsp_raw.txt"),
            (
                ArchiveObjectKind::GcRspParsed,
                "foo/bar/req-1/gc_rsp_parsed.json",
            ),
        ];
        for (kind, expected) in cases {
            assert_eq!(object_key("//foo//bar/", "req-1", kind), expected);
        }
    }

    #[test]
    fn gzip_roundtrip_and_hash_work() {
        let raw = br#"{\"hello\":\"world\"}"#;
        let gz = gzip_compress(raw).expect("compress");
        let back = gzip_decompress(&gz).expect("decompress");
        assert_eq!(back, raw);
        assert_eq!(sha256_hex(raw), sha256_hex(raw));
    }
}
