# S3 fixtures

Captured from a real S3-compatible server, so the S3 adapter in
`crates/integrations/src/s3.rs` is tested against what one says. Each file is
an HTTP status line, the response headers (the `Date` and request id
removed), and the body when it was short.

- `head-object-seaweedfs-4.48.txt`: HEAD of an object in a versioned bucket:
  `Content-Length`, a quoted `ETag`, `Last-Modified`, `X-Amz-Version-Id`.
- `get-object-range-old-version-seaweedfs-4.48.txt`: a GET of the object's
  older version (`versionId`) with `Range: bytes=100000-` and `If-Range` on
  that version's ETag: 206 with the rest.
- `get-object-missing-seaweedfs-4.48.txt`: 404 `NoSuchKey`.
- `get-object-wrong-key-seaweedfs-4.48.txt`: 403 `SignatureDoesNotMatch`, a
  request signed with the wrong secret.

## Provenance

SeaweedFS 4.48's S3 gateway, image `chrislusf/seaweedfs:4.48`, captured
2026-10-25 on a loopback-only port. MinIO, which the plan first named, no
longer publishes images publicly, so SeaweedFS (Apache-2.0) stands in for an
S3-compatible server here and in CI.

```bash
docker run -d -p 127.0.0.1:18333:8333 -v "$PWD/s3.json:/etc/seaweedfs/s3.json:ro" \
  chrislusf/seaweedfs:4.48 server -s3 -s3.config=/etc/seaweedfs/s3.json -s3.port=8333
```

`s3.json` named one identity with a throwaway key. A bucket was made, its
versioning turned on, and the same key written twice (200000 random bytes,
then 200100), with requests signed by a short SigV4 script.
