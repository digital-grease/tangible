# Metadata provider fixtures

What the metadata providers answer, for the clients in
`crates/integrations/src/metadata/` (ADR 0021, docs/plans/requests.md Q2).

- `musicbrainz-release-search.json`: RECORDED from musicbrainz.org on
  2026-11-13: `GET /ws/2/release?query=pink floyd dark side of the moon
  &limit=8&fmt=json&dismax=true`. Note what it holds: with `dismax=true` a
  plain query matches artists and titles together; the best-scored releases
  are tributes; there are cassettes, downloads and a DVD-Audio among the
  CDs, and a release with no media listed.
- `tmdb-search-movie.json`, `tmdb-search-tv.json`, `tmdb-find-imdb.json`,
  `tmdb-configuration.json`, `tmdb-unauthorized.json`: WRITTEN from TMDB's
  API reference (v3: `search/movie`, `search/tv`, `find/{external_id}` with
  `external_source=imdb_id`, `configuration`, and the 401 for a bad token),
  not recorded: no TMDB key was at hand. Record them again once one is.
  The movie search's third result carries markup in its title, and an empty
  release date, on purpose.
- `twitch-token.json`, `twitch-token-refused.json`, `igdb-games.json`,
  `igdb-unauthorized.json`: WRITTEN from Twitch's client-credentials flow
  and IGDB's API reference (v4 `POST /games`), not recorded, for the same
  reason. The third game has no release date and a `url` that is not
  IGDB's, on purpose. The token is not a real one.
