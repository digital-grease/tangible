# NZBGet JSON-RPC fixtures

Captured from a real NZBGet, downloading for real from a stand-in Usenet
server, so the adapter in `crates/integrations/src/nzbget.rs` is tested
against what the client says. The NZB is `../sabnzbd/probe.nzb`.

- `version-25.3.json`: `version`.
- `listgroups-25.3.json`: `listgroups` with one job waiting (added paused,
  to a category NZBGet does not have).
- `history-25.3.json`: `history` with the probe `SUCCESS/HEALTH` (its
  `DestDir` the folder holding the file), the same NZB added again
  `DELETED/COPY`, and a file that is not an NZB `FAILURE/SCAN`.

## Provenance

NZBGet 25.3, image `nzbgetcom/nzbget:v25.3`, captured 2026-10-25 on a
loopback-only port, with no route to any real news server:

```bash
docker run -d --name nzbget -p 127.0.0.1:18082:6789 \
  -e NZBGET_USER=tangible -e NZBGET_PASS=... -v "$PWD/config:/config" \
  nzbgetcom/nzbget:v25.3
```

The stand-in news server (the same Python NNTP responder as for SABnzbd) ran
in NZBGet's network namespace; `Server1` pointed at 127.0.0.1:1119 and
`Category5.Name=tangible` was added to nzbget.conf. The downloaded file was
byte-identical to the one served.

Observed along the way, and relied on by the adapter:

- JSON-RPC at `/jsonrpc` with HTTP basic authentication; a missing or wrong
  password answers 401 with an empty body.
- An unknown method answers an `error` object (`JSONRPCError`).
- `append` answers a positive `NZBID` for anything, even a file that is not
  an NZB or a category NZBGet does not have; the trouble shows later, in
  history (`FAILURE/SCAN`), or in a download filed under the unknown name.
- The same NZB added again is kept in history as `DELETED/COPY`.
- Sizes come as `FileSizeLo`/`FileSizeHi` (and `RemainingSizeLo`/`Hi` in the
  queue), 32-bit halves of the byte count; the `MB` fields round to 0 for a
  small file.
