use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{header, Client, Method, Url};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::ArchiveError;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct S3ArchiveClient {
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    force_path_style: bool,
    access_key_id: String,
    secret_access_key: String,
    timeout_secs: u64,
    http: Client,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("archive error: {0}")]
    Archive(#[from] ArchiveError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("s3 returned non-success status: {status} body: {body}")]
    Status { status: String, body: String },
    #[error("invalid s3 object url")]
    InvalidObjectUrl,
    #[error("signing error: {0}")]
    Signing(String),
}

pub trait PayloadArchiveClient: Send + Sync {
    fn bucket(&self) -> &str;
    fn prefix(&self) -> &str;
    fn timeout(&self) -> Duration;
    fn put_object<'a>(
        &'a self,
        key: &'a str,
        body: Bytes,
        content_type: &'a str,
        content_encoding: &'a str,
        metadata: Vec<(&'a str, &'a str)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ClientError>> + Send + 'a>>;
    fn get_object<'a>(
        &'a self,
        key: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Bytes, ClientError>> + Send + 'a>>;
}

impl S3ArchiveClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        force_path_style: bool,
        access_key_id: String,
        secret_access_key: String,
        timeout_secs: u64,
    ) -> Result<Self, ClientError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()?;
        Ok(Self {
            endpoint,
            region,
            bucket,
            prefix,
            force_path_style,
            access_key_id,
            secret_access_key,
            timeout_secs,
            http,
        })
    }

    fn object_url(&self, key: &str) -> Result<Url, ClientError> {
        let endpoint = Url::parse(self.endpoint.trim_end_matches('/'))
            .map_err(|_| ClientError::InvalidObjectUrl)?;
        let bucket = self.bucket.trim_matches('/');
        let key = key.trim_start_matches('/');
        if self.force_path_style {
            endpoint
                .join(&format!("{bucket}/{key}"))
                .map_err(|_| ClientError::InvalidObjectUrl)
        } else {
            let mut url = endpoint
                .join(key)
                .map_err(|_| ClientError::InvalidObjectUrl)?;
            let host = endpoint.host_str().ok_or(ClientError::InvalidObjectUrl)?;
            let host = match endpoint.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            };
            url.set_host(Some(&format!("{bucket}.{host}")))
                .map_err(|_| ClientError::InvalidObjectUrl)?;
            Ok(url)
        }
    }

    fn signed_headers(
        &self,
        method: Method,
        url: &Url,
        body: &[u8],
        extra: Vec<(&str, &str)>,
    ) -> Result<header::HeaderMap, ClientError> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = hex::encode(Sha256::digest(body));
        let host = canonical_host(url)?;

        let mut pairs: Vec<(String, String)> = vec![
            ("host".to_string(), host),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        for (k, v) in extra {
            pairs.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

        let canonical_headers = pairs
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect::<String>();
        let signed_headers = pairs
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            canonical_uri(url.path()),
            canonical_query(url.query()),
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let credential_scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let signing_key = signing_key(&self.secret_access_key, &date, &self.region)?;
        let signature = hmac_hex(&signing_key, string_to_sign.as_bytes())?;
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key_id, credential_scope, signed_headers, signature
        );

        let mut headers = header::HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            header::HeaderValue::from_str(&payload_hash)
                .map_err(|e| ClientError::Signing(e.to_string()))?,
        );
        headers.insert(
            "x-amz-date",
            header::HeaderValue::from_str(&amz_date)
                .map_err(|e| ClientError::Signing(e.to_string()))?,
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&authorization)
                .map_err(|e| ClientError::Signing(e.to_string()))?,
        );
        for (k, v) in pairs {
            if k == "host" || k == "x-amz-content-sha256" || k == "x-amz-date" {
                continue;
            }
            let name = header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| ClientError::Signing(e.to_string()))?;
            let value = header::HeaderValue::from_str(&v)
                .map_err(|e| ClientError::Signing(e.to_string()))?;
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

impl PayloadArchiveClient for S3ArchiveClient {
    fn bucket(&self) -> &str {
        &self.bucket
    }

    fn prefix(&self) -> &str {
        &self.prefix
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    fn put_object<'a>(
        &'a self,
        key: &'a str,
        body: Bytes,
        content_type: &'a str,
        content_encoding: &'a str,
        metadata: Vec<(&'a str, &'a str)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            let url = self.object_url(key)?;
            let mut extra = vec![
                ("content-type", content_type),
                ("content-encoding", content_encoding),
            ];
            let meta_headers = metadata
                .iter()
                .map(|(k, v)| (format!("x-amz-meta-{k}"), *v))
                .collect::<Vec<_>>();
            for (k, v) in &meta_headers {
                extra.push((k.as_str(), *v));
            }
            let headers = self.signed_headers(Method::PUT, &url, &body, extra)?;
            let res = self
                .http
                .put(url)
                .headers(headers)
                .body(body)
                .send()
                .await?;
            if !res.status().is_success() {
                let status = res.status().to_string();
                let body = res.text().await.unwrap_or_default();
                return Err(ClientError::Status { status, body });
            }
            Ok(())
        })
    }

    fn get_object<'a>(
        &'a self,
        key: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Bytes, ClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            let url = self.object_url(key)?;
            let headers = self.signed_headers(Method::GET, &url, &[], Vec::new())?;
            let res = self.http.get(url).headers(headers).send().await?;
            if !res.status().is_success() {
                let status = res.status().to_string();
                let body = res.text().await.unwrap_or_default();
                return Err(ClientError::Status { status, body });
            }
            Ok(res.bytes().await?)
        })
    }
}

fn canonical_host(url: &Url) -> Result<String, ClientError> {
    let host = url.host_str().ok_or(ClientError::InvalidObjectUrl)?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return "/".to_string();
    }
    let mut encoded = String::new();
    for segment in path.trim_start_matches('/').split('/') {
        encoded.push('/');
        if !segment.is_empty() {
            encoded.push_str(&encode_path_segment(segment));
        }
    }
    if path.ends_with('/') {
        encoded.push('/');
    }
    if encoded.is_empty() {
        "/".to_string()
    } else {
        encoded
    }
}

fn canonical_query(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };
    let mut pairs = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            (encode_path_segment(k), encode_path_segment(v))
        })
        .collect::<Vec<_>>();
    pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn signing_key(secret: &str, date: &str, region: &str) -> Result<Vec<u8>, ClientError> {
    let k_date = hmac_bytes(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k_region = hmac_bytes(&k_date, region.as_bytes())?;
    let k_service = hmac_bytes(&k_region, b"s3")?;
    hmac_bytes(&k_service, b"aws4_request")
}

fn hmac_bytes(key: &[u8], data: &[u8]) -> Result<Vec<u8>, ClientError> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|e| ClientError::Signing(e.to_string()))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hmac_hex(key: &[u8], data: &[u8]) -> Result<String, ClientError> {
    Ok(hex::encode(hmac_bytes(key, data)?))
}
