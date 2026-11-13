// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! An S3-compatible bucket an administrator configured
//! (docs/plans/acquisition.md A6, ADR 0016).
//!
//! Two things are asked of it: what an object is (HEAD: its size, ETag and
//! version), and the object itself, which the import runner fetches with
//! the URL import's resumable fetch from a short-lived presigned GET. A third
//! only where an administrator set the integration to remove after import and
//! the library's copy is checked: delete the version imported, only while its
//! ETag is the one imported (docs/plans/source-retention.md). Requests
//! are signed with SigV4 by `rusty-s3` (owner's choice, 2026-10-24) and sent
//! through the outbound client under the integrations policy: the endpoint is
//! the administrator's, so a private network is allowed.
//!
//! A presigned URL carries the access key id and a signature in its query.
//! It is made where it is used and never stored, logged or shown: the fetch
//! strips URLs from its errors and records a reference without the query.
//! The object key is the operator's, so it is checked, and never used as a
//! path; the file is staged under a name checked separately.
//!
//! Behaviour recorded from SeaweedFS 4.48's S3 gateway, in
//! fixtures/tool-output/s3: HEAD answers `Content-Length`, a quoted `ETag`,
//! `Last-Modified` and, in a versioned bucket, `X-Amz-Version-Id`; a missing
//! object or bucket is 404 (`NoSuchKey`, `NoSuchBucket`); a wrong key 403
//! (`SignatureDoesNotMatch`); a ranged GET of an older version with
//! `If-Range` on its ETag is 206. DELETE with `versionId` removes that
//! version, leaving the current one; with `If-Match` on another ETag it
//! answers 412 and deletes nothing; a key or version not there answers 204.

use std::fmt;
use std::time::Duration;

use rusty_s3::{Bucket, Credentials, S3Action as _, UrlStyle};
use tangible_domain::SecretString;

use crate::download::ClientError;
use crate::outbound::{OutboundClient, OutboundError, Url};

/// The client, in words, for errors.
const CLIENT: &str = "the S3 endpoint";

/// How long a presigned HEAD is good for: it is sent at once.
const HEAD_EXPIRY: Duration = Duration::from_mins(5);

/// How long a presigned GET is good for. S3 checks expiry when a request
/// starts, so a long transfer outlives it; a resumed one gets a new URL.
pub const GET_EXPIRY: Duration = Duration::from_hours(1);

/// The longest object key S3 allows, in bytes.
pub const MAX_KEY_BYTES: usize = 1024;

fn outbound(error: OutboundError) -> ClientError {
    ClientError::from_outbound(CLIENT, error)
}

/// What an object is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Its size in bytes.
    pub size: u64,
    /// Its ETag, quoted as the server sent it.
    pub etag: Option<String>,
    /// Its version, in a versioned bucket.
    pub version_id: Option<String>,
    /// When it was last changed, as the server wrote it.
    pub last_modified: Option<String>,
}

/// Why an object could not be had.
#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    /// There is no such object, or no such bucket.
    #[error("there is no such object in that bucket")]
    Missing,
    /// The endpoint could not be used.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// What a delete came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// Gone, or never there: S3 answers the same.
    Deleted,
    /// Its ETag is no longer the one given, so it was left.
    Changed,
    /// The endpoint cannot delete only if unchanged (501), so it was left.
    Unconditional,
}

/// Why a bucket name or object key was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct NameError(pub &'static str);

/// Check an object key: 1 to 1024 bytes, no control characters, and no `.`
/// or `..` segment. A dot segment is normalised away when the request URL
/// is built, so with buckets in the path `../other/x` would name an object
/// in bucket `other`: a way out of the allowlist.
///
/// # Errors
///
/// [`NameError`] saying why not.
pub fn check_key(key: &str) -> Result<(), NameError> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(NameError("an object key is 1 to 1024 bytes"));
    }
    if key.chars().any(char::is_control) {
        return Err(NameError("an object key has no control characters"));
    }
    if key
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(NameError("an object key has no '.' or '..' segments"));
    }
    Ok(())
}

/// A signed URL, if it still addresses `bucket`: the same scheme, host and
/// port as the bucket's own address, and a path beneath it. However a key
/// is spelled, a request never reaches another bucket or host.
fn confined(bucket: &Bucket, url: Url) -> Result<Url, ClientError> {
    let base = bucket.base_url();
    let base_path = if base.path().ends_with('/') {
        base.path().to_owned()
    } else {
        format!("{}/", base.path())
    };
    let same_origin = url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default();
    if same_origin && url.path().starts_with(&base_path) {
        Ok(url)
    } else {
        Err(ClientError::bad(
            CLIENT,
            "the request would leave the bucket; the key is not usable",
        ))
    }
}

/// Check a bucket name as S3 names them: 3 to 63 lowercase letters, digits,
/// `.` and `-`, starting and ending with a letter or digit.
///
/// # Errors
///
/// [`NameError`] saying why not.
pub fn check_bucket(name: &str) -> Result<(), NameError> {
    let edges_ok = name
        .bytes()
        .next()
        .zip(name.bytes().last())
        .is_some_and(|(first, last)| first.is_ascii_alphanumeric() && last.is_ascii_alphanumeric());
    let ok = (3..=63).contains(&name.len())
        && edges_ok
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(NameError(
            "a bucket name is 3 to 63 lowercase letters, digits, '.' and '-'",
        ))
    }
}

/// An S3-compatible endpoint and the credentials to sign for it with.
pub struct S3Client {
    http: OutboundClient,
    endpoint: Url,
    region: String,
    style: UrlStyle,
    credentials: Credentials,
}

impl fmt::Debug for S3Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Client")
            .field("endpoint", &crate::outbound::redacted(&self.endpoint))
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl S3Client {
    /// A client for the endpoint at `endpoint`, in `region`, addressing
    /// buckets in the path (`https://host/bucket/key`, as self-hosted
    /// servers usually want) or as a subdomain (as AWS does).
    #[must_use]
    pub fn new(
        http: OutboundClient,
        endpoint: Url,
        region: &str,
        path_style: bool,
        access_key_id: &SecretString,
        secret_access_key: &SecretString,
    ) -> Self {
        Self {
            http,
            endpoint,
            region: region.to_owned(),
            style: if path_style {
                UrlStyle::Path
            } else {
                UrlStyle::VirtualHost
            },
            credentials: Credentials::new(access_key_id.expose(), secret_access_key.expose()),
        }
    }

    fn bucket(&self, name: &str) -> Result<Bucket, ClientError> {
        Bucket::new(
            self.endpoint.clone(),
            self.style,
            name.to_owned(),
            self.region.clone(),
        )
        .map_err(|_| ClientError::bad(CLIENT, "the endpoint and bucket do not make an address"))
    }

    /// A presigned GET of `key` in `bucket`, pinned to `version_id` when
    /// given, good for [`GET_EXPIRY`]. Range and If-Range may be added: they
    /// are not signed.
    ///
    /// # Errors
    ///
    /// [`ClientError::BadResponse`] when no address can be made.
    pub fn presigned_get(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<Url, ClientError> {
        check_key(key).map_err(|error| ClientError::bad(CLIENT, error.0))?;
        let bucket = self.bucket(bucket)?;
        let mut action = bucket.get_object(Some(&self.credentials), key);
        if let Some(version) = version_id {
            action.query_mut().insert("versionId", version.to_owned());
        }
        confined(&bucket, action.sign(GET_EXPIRY))
    }

    /// Whether `bucket` is there and these credentials may use it.
    ///
    /// # Errors
    ///
    /// As [`Self::head`]; a bucket that is not there is
    /// [`ObjectError::Missing`].
    pub async fn head_bucket(&self, bucket: &str) -> Result<(), ObjectError> {
        let url = {
            let bucket = self.bucket(bucket)?;
            let signed = bucket
                .head_bucket(Some(&self.credentials))
                .sign(HEAD_EXPIRY);
            confined(&bucket, signed)?
        };
        self.send_head(&url).await.map(|_| ())
    }

    /// What `key` in `bucket` is.
    ///
    /// # Errors
    ///
    /// [`ObjectError::Missing`] for no such object or bucket;
    /// [`ObjectError::Client`] for a refusal (a wrong key is
    /// `INTEGRATION_AUTH_FAILED`) or an endpoint that cannot be reached or
    /// answers oddly.
    pub async fn head(&self, bucket: &str, key: &str) -> Result<ObjectInfo, ObjectError> {
        self.head_version(bucket, key, None).await
    }

    /// What `key` in `bucket` is, at `version_id` when given.
    ///
    /// # Errors
    ///
    /// As [`Self::head`].
    pub async fn head_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
    ) -> Result<ObjectInfo, ObjectError> {
        check_key(key).map_err(|error| ClientError::bad(CLIENT, error.0))?;
        let url = {
            let bucket = self.bucket(bucket)?;
            let mut action = bucket.head_object(Some(&self.credentials), key);
            if let Some(version) = version_id {
                action.query_mut().insert("versionId", version.to_owned());
            }
            confined(&bucket, action.sign(HEAD_EXPIRY))?
        };
        let headers = self.send_head(&url).await?;
        Ok(object_info(&headers)?)
    }

    /// Delete `key` in `bucket`: the version `version_id` when given, which
    /// leaves every other version, and only while its ETag is `if_match`
    /// when given.
    ///
    /// # Errors
    ///
    /// [`ObjectError::Missing`] for no such bucket; [`ObjectError::Client`]
    /// for a refusal (credentials that may not delete are
    /// `INTEGRATION_AUTH_FAILED`) or an endpoint that cannot be reached or
    /// answers oddly.
    pub async fn delete(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        if_match: Option<&str>,
    ) -> Result<Deleted, ObjectError> {
        check_key(key).map_err(|error| ClientError::bad(CLIENT, error.0))?;
        let url = {
            let bucket = self.bucket(bucket)?;
            let mut action = bucket.delete_object(Some(&self.credentials), key);
            if let Some(version) = version_id {
                action.query_mut().insert("versionId", version.to_owned());
            }
            if let Some(etag) = if_match {
                action.headers_mut().insert("if-match", etag.to_owned());
            }
            confined(&bucket, action.sign(HEAD_EXPIRY))?
        };
        let mut request = self
            .http
            .request(reqwest::Method::DELETE, &url)
            .map_err(outbound)?;
        if let Some(etag) = if_match {
            request = request.header("if-match", etag);
        }
        let response = self.http.send(request).await.map_err(outbound)?;
        delete_answer(response.status().as_u16())
    }

    /// Send a presigned HEAD, answering the headers of a 200.
    async fn send_head(&self, url: &Url) -> Result<reqwest::header::HeaderMap, ObjectError> {
        let request = self
            .http
            .request(reqwest::Method::HEAD, url)
            .map_err(outbound)?;
        let response = self.http.send(request).await.map_err(outbound)?;
        let status = response.status().as_u16();
        match status {
            200 => Ok(response.headers().clone()),
            404 => Err(ObjectError::Missing),
            403 => Err(ClientError::Auth {
                client: CLIENT,
                reason: "it refused the request; the access key, secret, region or bucket \
                         permissions are wrong",
            }
            .into()),
            301 | 307 | 400 => Err(ClientError::bad(
                CLIENT,
                format!(
                    "it answered {status}; the region or the addressing style (path or \
                     virtual host) may be wrong for this endpoint"
                ),
            )
            .into()),
            code => Err(ClientError::bad(CLIENT, format!("it answered {code}")).into()),
        }
    }
}

/// What a DELETE's status says.
///
/// # Errors
///
/// As [`S3Client::delete`].
pub fn delete_answer(status: u16) -> Result<Deleted, ObjectError> {
    match status {
        200 | 204 => Ok(Deleted::Deleted),
        412 => Ok(Deleted::Changed),
        501 => Ok(Deleted::Unconditional),
        404 => Err(ObjectError::Missing),
        403 => Err(ClientError::Auth {
            client: CLIENT,
            reason: "it refused to delete; the credentials need s3:DeleteObject, and \
                     s3:DeleteObjectVersion for a versioned bucket",
        }
        .into()),
        code => Err(ClientError::bad(CLIENT, format!("deleting answered {code}")).into()),
    }
}

/// What a HEAD answer says about an object.
///
/// # Errors
///
/// [`ClientError::BadResponse`] without a usable `Content-Length`.
pub fn object_info(headers: &reqwest::header::HeaderMap) -> Result<ObjectInfo, ClientError> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .map(str::to_owned)
    };
    let size = text("content-length")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ClientError::bad(CLIENT, "the object's size was not given"))?;
    Ok(ObjectInfo {
        size,
        etag: text("etag"),
        version_id: text("x-amz-version-id").filter(|version| version != "null"),
        last_modified: text("last-modified"),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The headers of a recorded answer: lines `Name: value` after the
    /// status line, up to the body marker.
    fn recorded(name: &str) -> (u16, reqwest::header::HeaderMap) {
        let text = std::fs::read_to_string(format!(
            "{}/../../fixtures/tool-output/s3/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut lines = text.lines();
        let status = lines.next().unwrap().trim().parse().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        for line in lines.take_while(|line| !line.starts_with("-- body")) {
            let (name, value) = line.split_once(": ").unwrap();
            headers.append(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        (status, headers)
    }

    #[test]
    fn recorded_deletes_read_as_deleted_or_left_and_refusals_say_why() {
        for (name, answer) in [
            ("delete-version-seaweedfs-4.48.txt", Deleted::Deleted),
            ("delete-version-gone-seaweedfs-4.48.txt", Deleted::Deleted),
            (
                "delete-if-match-other-etag-seaweedfs-4.48.txt",
                Deleted::Changed,
            ),
        ] {
            let (status, _) = recorded(name);
            assert_eq!(delete_answer(status).unwrap(), answer, "{name}");
        }
        assert_eq!(delete_answer(501).unwrap(), Deleted::Unconditional);
        assert!(matches!(delete_answer(404), Err(ObjectError::Missing)));
        let refused = delete_answer(403).unwrap_err().to_string();
        assert!(refused.contains("s3:DeleteObject"), "{refused}");
        let odd = delete_answer(500).unwrap_err().to_string();
        assert!(odd.contains("500"), "{odd}");
    }

    #[test]
    fn a_recorded_head_reads_as_size_etag_and_version() {
        let (status, headers) = recorded("head-object-seaweedfs-4.48.txt");
        assert_eq!(status, 200);
        let info = object_info(&headers).unwrap();
        assert_eq!(info.size, 200_100);
        assert_eq!(
            info.etag.as_deref(),
            Some("\"ba5e31b5b5bb1289d3a62746e60b0cf9\"")
        );
        assert_eq!(
            info.version_id.as_deref(),
            Some("671e1b346e2cdf318cb0fad7179d937c")
        );
        assert!(info.last_modified.is_some());
    }

    #[test]
    fn a_head_without_a_size_will_not_do_and_a_null_version_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert!(object_info(&headers).is_err());
        headers.insert("content-length", "5".parse().unwrap());
        headers.insert("x-amz-version-id", "null".parse().unwrap());
        assert_eq!(object_info(&headers).unwrap().version_id, None);
    }

    #[test]
    fn keys_and_buckets_are_checked() {
        assert!(check_key("discs/Probe Disc.iso").is_ok());
        assert!(check_key("").is_err());
        assert!(check_key(&"k".repeat(1025)).is_err());
        assert!(check_key("a\nb").is_err());
        for escape in ["../other/x.iso", "a/../../other/x", "./x", "a/./b", ".."] {
            assert!(check_key(escape).is_err(), "{escape}");
        }
        assert!(
            check_key("a..b/.hidden/x...iso").is_ok(),
            "dots inside a segment are fine"
        );
        for good in ["tangible-test", "my.bucket.01", "abc"] {
            assert!(check_bucket(good).is_ok(), "{good}");
        }
        for bad in ["ab", "Tangible", "-x-", "a..b", "a_b", &"a".repeat(64)] {
            assert!(check_bucket(bad).is_err(), "{bad}");
        }
    }

    fn client(endpoint: &str, path_style: bool) -> S3Client {
        S3Client::new(
            OutboundClient::new(
                crate::outbound::OutboundPolicy::for_integrations(Vec::new(), None),
                "tangible-test",
            )
            .unwrap(),
            Url::parse(endpoint).unwrap(),
            "us-east-1",
            path_style,
            &SecretString::from("AKIDEXAMPLE".to_owned()),
            &SecretString::from("a-secret-that-never-appears".to_owned()),
        )
    }

    #[test]
    fn no_key_reaches_another_bucket() {
        // Unchecked, a dot segment is normalised away and this names
        // /other/x.iso: bucket `other`.
        let path_style = client("http://192.168.1.13:8333/", true);
        let bucket = path_style.bucket("allowed").unwrap();
        let escaped = bucket
            .get_object(Some(&path_style.credentials), "../other/x.iso")
            .sign(GET_EXPIRY);
        assert_eq!(escaped.path(), "/other/x.iso");
        assert!(
            confined(&bucket, escaped).is_err(),
            "caught after signing too"
        );
        assert!(
            path_style
                .presigned_get("allowed", "../other/x.iso", None)
                .is_err()
        );
        let virtual_host = client("https://s3.example.internal/", false);
        assert!(
            virtual_host
                .presigned_get("allowed", "../x.iso", None)
                .is_err()
        );
        assert!(
            path_style
                .presigned_get("allowed", "dir/x.iso", None)
                .is_ok()
        );
        assert!(
            virtual_host
                .presigned_get("allowed", "dir/x.iso", None)
                .is_ok()
        );
    }

    #[test]
    fn a_presigned_get_is_pinned_to_its_version_and_never_carries_the_secret() {
        let url = client("http://192.168.1.13:8333/", true)
            .presigned_get("tangible-test", "discs/Probe Disc.iso", Some("v1"))
            .unwrap();
        assert_eq!(url.host_str(), Some("192.168.1.13"));
        assert!(
            url.path()
                .starts_with("/tangible-test/discs/Probe%20Disc.iso"),
            "{url}"
        );
        let query = url.query().unwrap();
        assert!(query.contains("versionId=v1"), "{query}");
        assert!(query.contains("X-Amz-Signature="), "{query}");
        assert!(!url.as_str().contains("a-secret-that-never-appears"));
        assert!(
            !format!("{:?}", client("http://192.168.1.13:8333/", true)).contains("AKIDEXAMPLE")
        );

        let virtual_host = client("https://s3.example.internal/", false)
            .presigned_get("tangible-test", "x.iso", None)
            .unwrap();
        assert_eq!(
            virtual_host.host_str(),
            Some("tangible-test.s3.example.internal")
        );
        // A bucket cannot be a subdomain of an address.
        assert!(
            client("http://192.168.1.13:8333/", false)
                .presigned_get("tangible-test", "x.iso", None)
                .is_err()
        );
    }
}
