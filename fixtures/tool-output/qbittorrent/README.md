# qBittorrent WebUI fixtures

Captured from a real qBittorrent, so the adapter in
`crates/integrations/src/qbittorrent.rs` is tested against what the client
says rather than against its documentation.

- `single-file.torrent`, `multi-file.torrent`: private, trackerless torrents
  of random files (300000 bytes; a folder `Probe Disc (CUE)` holding a
  100000-byte `disc.bin` and a three-line `disc.cue`). They reach no peer and
  name nothing anyone could want. `crates/integrations/src/torrent.rs` checks
  that their infohashes are the `hash` qBittorrent reported.
- `torrents-info-5.1.2.json`: `GET /api/v2/torrents/info?category=tangible`
  with both torrents finished (their files were already in the category's
  folder, so qBittorrent checked them and went straight to `stalledUP`) and a
  magnet link for a hash nobody has, still fetching metadata (`metaDL`, empty
  `content_path`).

## Provenance

qBittorrent 5.1.2 (WebUI API 2.11.4), image
`qbittorrentofficial/qbittorrent-nox:5.1.2-1`, captured 2026-10-25 on a
loopback-only port:

```bash
docker run -d --name qbt -e QBT_LEGAL_NOTICE=confirm -e QBT_WEBUI_PORT=18080 \
  -p 127.0.0.1:18080:18080 -v "$PWD/downloads:/downloads" \
  qbittorrentofficial/qbittorrent-nox:5.1.2-1
docker logs qbt   # the temporary administrator password
```

The files were placed in `downloads/tangible/` first, the torrents made from
them with a small bencoder, and added with `category=tangible`,
`autoTMM=true`, `stopped=false`.

Observed along the way, and relied on by the adapter:

- `auth/login` answers 200 `Ok.` and an `SID` cookie, or 200 `Fails.` for a
  wrong password.
- With host header validation on (the default), a request whose `Host` port
  differs from the WebUI's own (a remapped Docker port) is refused with 401
  before the password is looked at.
- The API answers 401 without a session and 403 with a stale one.
- `torrents/add` answers 200 `Ok.`, or 200 `Fails.` for a torrent it already
  has, and makes a category that does not exist yet.
- `torrents/delete` (checked 2026-10-27, same image) takes only a POST (405
  for a GET) and needs `deleteFiles` (400 without it). It answers 200 with an
  empty body whether or not it had the torrent. With `deleteFiles=true` the
  torrent is gone from `torrents/info` at once and its files a moment later;
  with `false` the files stay.
