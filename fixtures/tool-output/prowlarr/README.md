# Prowlarr API fixtures

Recorded from a real Prowlarr searching a stand-in indexer, so the client
in `crates/integrations/src/prowlarr.rs` and the request search in
`crates/api/tests/request_releases.rs` are tested against what Prowlarr
says (ADR 0021, docs/plans/requests.md Q3).

- `system-status-2.7.0.json`: `GET /api/v1/system/status`.
- `indexers-2.7.0.json`: `GET /api/v1/indexer`, a Generic Torznab and a
  Generic Newznab indexer. Prowlarr answers an indexer's own API key as
  `********`.
- `indexer-status-failing-2.7.0.json`: `GET /api/v1/indexerstatus` once
  both indexers had failed: each paused, with `disabledTill` a minute on.
- `health-failing-2.7.0.json`: `GET /api/v1/health` then: "All indexers
  are unavailable due to failures".
- `search-movies-2.7.0.json`: `GET /api/v1/search?query=Big Buck
  Bunny&type=search&categories=2000&limit=100`: two torrents and one NZB.
- `search-imdb-2.7.0.json`: the same film by IMDb ID,
  `query={ImdbId:tt1254207}&type=movie&categories=2000`, which Prowlarr sent
  each indexer as `t=movie&imdbid=1254207`.
- `search-magnet-only-2.7.0.json`: a torrent with only a magnet link,
  `categories=3000`.
- `search-usenet-2.7.0.json`: an NZB in `categories=1000&categories=4000`,
  which Prowlarr sent as the categories and their subcategories.
- `search-refused-2.7.0.json`: the 400 for `indexerIds=999`, an indexer it
  does not have. A search naming only paused indexers answers the same.

Every release's `downloadUrl` and `magnetUrl` is Prowlarr's own link, with
its API key in the query; in these files the key is `REDACTED-API-KEY`,
and nothing else was changed. Tests point the links back at a fake.

## Provenance

Prowlarr 2.7.0.5723, image `lscr.io/linuxserver/prowlarr` (build
`2.7.0.5723-ls168`, digest
`sha256:6660787a99b2e58203539583663c973b48979fc00ff2cb78dc0ea17429e1f6ef`),
recorded 2026-11-14 on loopback only, with a throwaway API key, no
download client and no route to any real indexer:

```bash
docker run -d --name prowlarr --network host \
  -e PROWLARR__SERVER__PORT=19696 -e PROWLARR__SERVER__BINDADDRESS=127.0.0.1 \
  -e PROWLARR__AUTH__APIKEY=<a throwaway key> \
  -v "$PWD/config:/config" lscr.io/linuxserver/prowlarr@sha256:6660787a...
```

The stand-in indexer was a small Python server on 127.0.0.1:18990 speaking
Torznab at `/torznab/api` and Newznab at `/newznab/api` (caps, searches and
downloads), serving public-domain and Creative Commons titles only: Big
Buck Bunny, its soundtrack, Nosferatu (1922) and a homebrew collection.
Its `.torrent` files and NZBs were made up for the recording and name no
real content. The two indexers were added through `POST /api/v1/indexer`
from the Generic Torznab and Generic Newznab schemas.

Observed along the way, and relied on by the client:

- Without the right key (or with none), every route answers 401 with an
  empty body. The key goes in the `X-Api-Key` header.
- A Torznab item without an `<enclosure>` makes Prowlarr's parser fail on
  it (a NullReferenceException in its log); the item is dropped. Magnet-only
  feeds give the magnet link as the enclosure, as the stand-in then did.
- A release's `downloadUrl` answers, without the header (the link carries
  the key): the `.torrent` itself (200 `application/x-bittorrent`) for a
  torrent with one; a 301 to the `magnet:` link for a magnet-only one; and,
  for an NZB, a 301 to the indexer's own link, with the indexer's key in
  it. Prowlarr refuses to turn redirecting off for a Usenet indexer
  ("Redirect must be enabled for Usenet indexers"), so an NZB always comes
  through that redirect. The same link with its key changed answers 401.
- `leechers` is `peers` less `seeders` from the Torznab attributes.
  Categories come with the indexer's own numbers (100000 and up) beside the
  standard ones; those have no name.
- An indexer that fails answers nothing: the search is a 200 with what the
  others found (or `[]`), and Prowlarr pauses the indexer. A search of
  every indexer while all are paused is a 200 `[]`.
- `type=bogus` is not refused: it answers `[]`.
