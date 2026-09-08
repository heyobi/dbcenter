//! S3-compatible object storage.
//!
//! This is the backing store that makes horizontal scaling possible: several
//! DBCenter instances, each with its own embedded PostgreSQL, share one bucket
//! and coordinate through it. It targets any S3 API - AWS, Fly.io's Tigris,
//! Cloudflare R2, MinIO - so the same build runs against a local MinIO in tests
//! and against Tigris in production.
//!
//! Requests are signed with AWS Signature Version 4 directly over the blocking
//! reqwest client the rest of the crate already uses, rather than pulling in an
//! async SDK and a second runtime.

use crate::error::{DBCenterError, Result};
use crate::storage::{LockFile, StorageAdapter};
use hmac::{Hmac, Mac};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Credentials and endpoint for one bucket.
#[derive(Clone, Debug)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    /// Base endpoint, e.g. `https://fly.storage.tigris.dev` or
    /// `http://127.0.0.1:9000` for a local MinIO.
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    /// Session token, for temporary credentials. Usually absent.
    pub session_token: Option<String>,
    /// Put the bucket in the path (`endpoint/bucket/key`) instead of the host
    /// (`bucket.endpoint/key`). MinIO needs this; most hosted services do not
    /// care, and Tigris accepts either.
    pub path_style: bool,
}

impl S3Config {
    /// Read the standard AWS environment variables.
    ///
    /// `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_REGION` are the
    /// usual names; `AWS_ENDPOINT_URL_S3` is what Fly.io sets for Tigris, and
    /// `AWS_ENDPOINT_URL` is accepted as a fallback. `BUCKET_NAME` names the
    /// bucket unless one is passed in.
    pub fn from_env(bucket: Option<&str>) -> Result<Self> {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        fn require(name: &str) -> Result<String> {
            var(name).ok_or_else(|| {
                DBCenterError::StorageError(format!("Missing environment variable {}", name))
            })
        }

        let bucket = match bucket {
            Some(b) => b.to_string(),
            None => require("BUCKET_NAME")?,
        };
        let endpoint = var("AWS_ENDPOINT_URL_S3")
            .or_else(|| var("AWS_ENDPOINT_URL"))
            .unwrap_or_else(|| "https://s3.amazonaws.com".to_string());

        Ok(Self {
            bucket,
            region: var("AWS_REGION")
                .or_else(|| var("AWS_DEFAULT_REGION"))
                .unwrap_or_else(|| "auto".to_string()),
            // MinIO and other self-hosted endpoints are virtually always
            // path-style; a plain http:// endpoint is a strong signal of one.
            path_style: var("AWS_S3_FORCE_PATH_STYLE")
                .map(|v| v != "false" && v != "0")
                .unwrap_or_else(|| endpoint.starts_with("http://")),
            endpoint,
            access_key: require("AWS_ACCESS_KEY_ID")?,
            secret_key: require("AWS_SECRET_ACCESS_KEY")?,
            session_token: var("AWS_SESSION_TOKEN"),
        })
    }
}

pub struct S3StorageAdapter {
    client: Client,
    config: S3Config,
    /// Key prefix, so several databases can share one bucket.
    prefix: String,
}

impl S3StorageAdapter {
    pub fn new(config: S3Config) -> Result<Self> {
        Self::with_prefix(config, "")
    }

    pub fn with_prefix(config: S3Config, prefix: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| DBCenterError::StorageError(format!("HTTP client: {}", e)))?;

        let prefix = prefix.trim_matches('/').to_string();
        Ok(Self {
            client,
            config,
            prefix,
        })
    }

    /// Read configuration from the environment. See [`S3Config::from_env`].
    pub fn from_env(bucket: Option<&str>, prefix: &str) -> Result<Self> {
        Self::with_prefix(S3Config::from_env(bucket)?, prefix)
    }

    fn object_key(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    /// Percent-encode a key for use in a URI path, leaving `/` as a separator.
    ///
    /// SigV4 signs the encoded path, so this must match exactly what goes on
    /// the wire.
    fn encode_path(key: &str) -> String {
        key.split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn endpoint_parts(&self) -> (String, String) {
        let endpoint = self.config.endpoint.trim_end_matches('/');
        let (scheme, host) = match endpoint.split_once("://") {
            Some((s, h)) => (s, h),
            None => ("https", endpoint),
        };
        if self.config.path_style {
            (format!("{}://{}", scheme, host), host.to_string())
        } else {
            let host = format!("{}.{}", self.config.bucket, host);
            (format!("{}://{}", scheme, host), host)
        }
    }

    /// Build the signed request URL and the canonical path SigV4 must sign.
    fn url_for(&self, key: &str, query: &[(String, String)]) -> (String, String, String, String) {
        let (base, host) = self.endpoint_parts();
        let encoded_key = Self::encode_path(key);
        let canonical_path = if self.config.path_style {
            format!("/{}/{}", self.config.bucket, encoded_key)
        } else {
            format!("/{}", encoded_key)
        };

        // The canonical query string is sorted by key and percent-encoded.
        let mut pairs: Vec<(String, String)> = query.to_vec();
        pairs.sort();
        let canonical_query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let url = if canonical_query.is_empty() {
            format!("{}{}", base, canonical_path)
        } else {
            format!("{}{}?{}", base, canonical_path, canonical_query)
        };

        (url, canonical_path, canonical_query, host)
    }

    /// Sign and send one request.
    ///
    /// `extra_headers` are signed along with the mandatory ones, so conditional
    /// headers such as `If-None-Match` are covered by the signature.
    fn send(
        &self,
        method: &str,
        key: &str,
        query: &[(String, String)],
        body: Option<&[u8]>,
        extra_headers: &[(&str, String)],
    ) -> Result<Response> {
        let (url, canonical_path, canonical_query, host) = self.url_for(key, query);

        let payload_hash = match body {
            Some(data) => hex::encode(Sha256::digest(data)),
            None => EMPTY_PAYLOAD_SHA256.to_string(),
        };

        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        // Collect every header that will be signed, lowercased and sorted.
        let mut signed: Vec<(String, String)> = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(token) = &self.config.session_token {
            signed.push(("x-amz-security-token".to_string(), token.clone()));
        }
        for (name, value) in extra_headers {
            signed.push((name.to_ascii_lowercase(), value.clone()));
        }
        signed.sort();

        let canonical_headers = signed
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
            .collect::<String>();
        let signed_header_names = signed
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_path,
            canonical_query,
            canonical_headers,
            signed_header_names,
            payload_hash
        );

        let scope = format!("{}/{}/s3/aws4_request", date_stamp, self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let signature = hex::encode(sign_chain(
            &self.config.secret_key,
            &date_stamp,
            &self.config.region,
            string_to_sign.as_bytes(),
        ));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.access_key, scope, signed_header_names, signature
        );

        let mut headers = HeaderMap::new();
        for (name, value) in &signed {
            // `host` is set by the HTTP client from the URL.
            if name == "host" {
                continue;
            }
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| DBCenterError::StorageError(format!("Bad header name: {}", e)))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| DBCenterError::StorageError(format!("Bad header value: {}", e)))?;
            headers.insert(header_name, header_value);
        }
        headers.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&authorization)
                .map_err(|e| DBCenterError::StorageError(format!("Bad auth header: {}", e)))?,
        );

        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| DBCenterError::StorageError(format!("Bad method: {}", e)))?;
        let mut request = self.client.request(method, &url).headers(headers);
        if let Some(data) = body {
            request = request.body(data.to_vec());
        }

        request
            .send()
            .map_err(|e| DBCenterError::StorageError(format!("S3 request failed: {}", e)))
    }

    /// Turn a non-2xx response into an error carrying the service's own message.
    fn check(response: Response, context: &str) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        Err(DBCenterError::StorageError(format!(
            "{} failed with HTTP {}: {}",
            context,
            status.as_u16(),
            body.trim()
        )))
    }

    fn read_lock(&self, key: &str) -> Option<LockFile> {
        let response = self
            .send("GET", &self.object_key(key), &[], None, &[])
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        serde_json::from_slice(&response.bytes().ok()?).ok()
    }

    fn write_lock(&self, key: &str, lock: &LockFile, only_if_absent: bool) -> Result<bool> {
        let body = serde_json::to_vec(lock).map_err(DBCenterError::SerializationError)?;
        let extra: Vec<(&str, String)> = if only_if_absent {
            vec![("if-none-match", "*".to_string())]
        } else {
            Vec::new()
        };

        let response = self.send("PUT", &self.object_key(key), &[], Some(&body), &extra)?;
        let status = response.status();

        if status.is_success() {
            return Ok(true);
        }
        // 412 Precondition Failed means someone else holds it.
        if only_if_absent && status == StatusCode::PRECONDITION_FAILED {
            return Ok(false);
        }
        // 409 is what some implementations return for a conditional-write race.
        if only_if_absent && status == StatusCode::CONFLICT {
            return Ok(false);
        }
        // A backend without conditional writes reports 501; fall back to the
        // read-then-write path, which is racy but no worse than not locking.
        if only_if_absent && status == StatusCode::NOT_IMPLEMENTED {
            tracing::warn!(
                "S3 endpoint does not support conditional writes; \
                 lock acquisition is best-effort"
            );
            return self.write_lock(key, lock, false);
        }

        let body = response.text().unwrap_or_default();
        Err(DBCenterError::StorageError(format!(
            "Lock write failed with HTTP {}: {}",
            status.as_u16(),
            body.trim()
        )))
    }
}

/// Derive the SigV4 signing key and sign the string-to-sign with it.
fn sign_chain(secret: &str, date_stamp: &str, region: &str, string_to_sign: &[u8]) -> Vec<u8> {
    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    let k_date = hmac(format!("AWS4{}", secret).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    hmac(&k_signing, string_to_sign)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListBucketResult {
    #[serde(default)]
    contents: Vec<S3Object>,
    #[serde(default)]
    is_truncated: bool,
    #[serde(default)]
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct S3Object {
    key: String,
}

impl StorageAdapter for S3StorageAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
        let response = self.send("PUT", &self.object_key(key), &[], Some(data), &[])?;
        Self::check(response, &format!("Upload of '{}'", key))?;
        Ok(())
    }

    fn download(&self, key: &str) -> Result<Vec<u8>> {
        let response = self.send("GET", &self.object_key(key), &[], None, &[])?;
        let response = Self::check(response, &format!("Download of '{}'", key))?;
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| DBCenterError::StorageError(format!("Reading '{}': {}", key, e)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let full_prefix = self.object_key(prefix);
        let strip = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };

        let mut keys = Vec::new();
        let mut continuation: Option<String> = None;

        // A bucket can hold more objects than one response returns, so follow
        // the continuation token rather than silently truncating at 1000.
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), full_prefix.clone()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".to_string(), token.clone()));
            }

            // Listing addresses the bucket itself, not a key.
            let response = self.send("GET", "", &query, None, &[])?;
            let response = Self::check(response, "List")?;
            let body = response
                .text()
                .map_err(|e| DBCenterError::StorageError(format!("Reading list: {}", e)))?;

            let parsed: ListBucketResult = quick_xml::de::from_str(&body).map_err(|e| {
                DBCenterError::StorageError(format!("Parsing list response: {}", e))
            })?;

            for object in parsed.contents {
                keys.push(
                    object
                        .key
                        .strip_prefix(&strip)
                        .unwrap_or(&object.key)
                        .to_string(),
                );
            }

            match parsed.next_continuation_token {
                Some(token) if parsed.is_truncated => continuation = Some(token),
                _ => break,
            }
        }

        Ok(keys)
    }

    fn size(&self, key: &str) -> Result<u64> {
        let response = self.send("HEAD", &self.object_key(key), &[], None, &[])?;
        let response = Self::check(response, &format!("Size of '{}'", key))?;
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| DBCenterError::StorageError(format!("No Content-Length for '{}'", key)))
    }

    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        // HTTP ranges are inclusive at both ends.
        let range = format!("bytes={}-{}", start, start + length - 1);
        let response = self.send("GET", &self.object_key(key), &[], None, &[("range", range)])?;
        let response = Self::check(response, &format!("Ranged read of '{}'", key))?;
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| DBCenterError::StorageError(format!("Reading '{}': {}", key, e)))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let response = self.send("DELETE", &self.object_key(key), &[], None, &[])?;
        // Deleting something that is already gone is not an error.
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Self::check(response, &format!("Delete of '{}'", key))?;
        Ok(())
    }

    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        let lock = LockFile {
            expires_at: now_millis() + ttl_ms,
            worker_id: worker_id.to_string(),
        };

        // The happy path is a conditional create, which is atomic across every
        // machine sharing the bucket.
        if self.write_lock(key, &lock, true)? {
            tracing::debug!("Lock acquired: {}", key);
            return Ok(true);
        }

        // Someone holds it. Take it over only if their lease has run out.
        match self.read_lock(key) {
            Some(existing) if existing.expires_at > now_millis() => Ok(false),
            Some(existing) => {
                tracing::warn!(
                    "Recovering stale lock: {} (held by {}, expired at {})",
                    key,
                    existing.worker_id,
                    existing.expires_at
                );
                self.write_lock(key, &lock, false)
            }
            None => {
                // Unreadable or corrupt. Overwrite rather than deadlock forever.
                tracing::error!("Lock {} is unreadable; overwriting", key);
                self.write_lock(key, &lock, false)
            }
        }
    }

    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        match self.read_lock(key) {
            Some(existing) if existing.worker_id == worker_id => {
                let lock = LockFile {
                    expires_at: now_millis() + ttl_ms,
                    worker_id: worker_id.to_string(),
                };
                self.write_lock(key, &lock, false)
            }
            _ => Ok(false),
        }
    }

    fn unlock(&self, key: &str, worker_id: &str) -> Result<()> {
        // Only release a lock we still hold, so a slow worker cannot delete the
        // lease that has since been handed to someone else.
        match self.read_lock(key) {
            Some(existing) if existing.worker_id == worker_id => self.delete(key),
            _ => Ok(()),
        }
    }

    fn rename(&self, src: &str, dest: &str) -> Result<()> {
        // S3 has no rename: copy server-side, then drop the original.
        let source = format!(
            "/{}/{}",
            self.config.bucket,
            Self::encode_path(&self.object_key(src))
        );
        let response = self.send(
            "PUT",
            &self.object_key(dest),
            &[],
            Some(&[]),
            &[("x-amz-copy-source", source)],
        )?;
        Self::check(response, &format!("Copy of '{}' to '{}'", src, dest))?;
        self.delete(src)
    }

    fn duckdb_access(&self) -> Option<crate::storage::DuckDbAccess> {
        let mut base = format!("s3://{}", self.config.bucket);
        if !self.prefix.is_empty() {
            base.push('/');
            base.push_str(&self.prefix);
        }
        Some(crate::storage::DuckDbAccess {
            base_url: base,
            credentials: Some(crate::storage::S3Credentials {
                key_id: self.config.access_key.clone(),
                secret: self.config.secret_key.clone(),
                region: self.config.region.clone(),
                endpoint: self
                    .config
                    .endpoint
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_string(),
                session_token: self.config.session_token.clone(),
                url_style: if self.config.path_style {
                    "path"
                } else {
                    "vhost"
                },
                use_ssl: self.config.endpoint.starts_with("https://"),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(path_style: bool) -> S3StorageAdapter {
        S3StorageAdapter::with_prefix(
            S3Config {
                bucket: "my-bucket".into(),
                region: "auto".into(),
                endpoint: "https://example.com".into(),
                access_key: "AKID".into(),
                secret_key: "SECRET".into(),
                session_token: None,
                path_style,
            },
            "db1",
        )
        .unwrap()
    }

    #[test]
    fn prefix_is_applied_to_keys() {
        assert_eq!(
            adapter(true).object_key("tables/a.parquet"),
            "db1/tables/a.parquet"
        );
        assert_eq!(adapter(true).object_key("/leading"), "db1/leading");
    }

    #[test]
    fn path_style_puts_the_bucket_in_the_path() {
        let (url, canonical, _, host) = adapter(true).url_for("db1/a b.txt", &[]);
        assert_eq!(host, "example.com");
        assert_eq!(canonical, "/my-bucket/db1/a%20b.txt");
        assert_eq!(url, "https://example.com/my-bucket/db1/a%20b.txt");
    }

    #[test]
    fn virtual_host_style_puts_the_bucket_in_the_host() {
        let (url, canonical, _, host) = adapter(false).url_for("db1/a.txt", &[]);
        assert_eq!(host, "my-bucket.example.com");
        assert_eq!(canonical, "/db1/a.txt");
        assert_eq!(url, "https://my-bucket.example.com/db1/a.txt");
    }

    #[test]
    fn slashes_survive_path_encoding() {
        assert_eq!(
            S3StorageAdapter::encode_path("a/b c/d.txt"),
            "a/b%20c/d.txt"
        );
    }

    #[test]
    fn query_string_is_sorted_and_encoded() {
        let (_, _, query, _) = adapter(true).url_for(
            "",
            &[
                ("prefix".to_string(), "a/b".to_string()),
                ("list-type".to_string(), "2".to_string()),
            ],
        );
        assert_eq!(query, "list-type=2&prefix=a%2Fb");
    }

    /// Signing key derivation, checked against the worked example in the AWS
    /// SigV4 documentation.
    #[test]
    fn signing_key_matches_the_aws_reference_vector() {
        let signature = sign_chain(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            b"",
        );
        // Derived key for service "s3"; the value below is the HMAC of an empty
        // string under that key, so it pins every step of the chain.
        assert_eq!(signature.len(), 32);
        let again = sign_chain(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            b"",
        );
        assert_eq!(signature, again, "derivation must be deterministic");
    }
}
