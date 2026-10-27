# RomM API fixtures

Captured from a real RomM, so the client in
`crates/integrations/src/romm_api.rs` is tested against what RomM says.

- `heartbeat-5.4.1.json`: `GET /api/heartbeat`, no token: the version and
  the instance's configuration.
- `openapi-5.4.1-used.json`: RomM's `GET /openapi.json` (494 KB), cut down
  to the operations Tangible uses, each with its parameters' names, as
  below.
- `platforms-5.4.1.json`: `GET /api/platforms`, the one platform the library
  had (`psx`).
- `roms-limit-1-5.4.1.json`: `GET /api/roms?limit=1`, the request that
  proves the token carries `roms.read`.
- `roms-search-5.4.1.json`: `GET /api/roms?platform_ids=1&search_term=Probe
  Game (USA)&with_files=true`: a two-file game in its own folder.
- `scan-queued-5.4.1.json`: `POST /api/tasks/scan` with
  `{"type": "quick", "platform_fs_slugs": ["psx"]}`, answering 202.
- `task-early-5.4.1.json`, `task-finished-5.4.1.json`: `GET
  /api/tasks/{task_id}` for that scan, `started` and then `finished` with
  its counts.

## Provenance

RomM 5.4.1, image `rommapp/romm:5.4.1`, with `mariadb:11.4`, captured
2026-10-26 on a loopback-only port, with no metadata provider configured:

```yaml
name: romm-probe
services:
  db:
    image: mariadb:11.4
    environment:
      MARIADB_ROOT_PASSWORD: probe-root
      MARIADB_DATABASE: romm
      MARIADB_USER: romm
      MARIADB_PASSWORD: probe-pass
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 5s
      retries: 30
  romm:
    image: rommapp/romm:5.4.1
    depends_on:
      db:
        condition: service_healthy
    environment:
      DB_HOST: db
      DB_NAME: romm
      DB_USER: romm
      DB_PASSWD: probe-pass
      ROMM_AUTH_SECRET_KEY: <64 hex characters>
    volumes:
      - ./library:/romm/library
      - ./assets:/romm/assets
      - ./config:/romm/config
      - ./resources:/romm/resources
    ports:
      - "127.0.0.1:18080:8080"
```

The first user was made through the setup flow (`POST /api/users` with the
CSRF cookie from the heartbeat), then a client token with
`POST /api/client-tokens`, `{"name": "tangible", "scopes":
["platforms.read", "roms.read", "tasks.run"]}`. The game was
`library/roms/psx/Probe Game (USA)/`, holding `Probe Game (Disc 1).bin`
(the bytes 0 to 255 repeated, 2408448 bytes) and a one-track
`Probe Game (Disc 1).cue`.

The document was cut down with:

```python
keep = {"/api/heartbeat": ["get"], "/api/platforms": ["get"],
        "/api/platforms/supported": ["get"], "/api/roms": ["get"],
        "/api/roms/{id}": ["get"], "/api/tasks": ["get"],
        "/api/tasks/scan": ["post"], "/api/tasks/{task_id}": ["get"],
        "/api/client-tokens": ["get", "post"]}
# each operation without its responses and description, and each
# parameter as only its name, in and required
```

Observed along the way, and relied on by the client:

- `/api/heartbeat` and `/openapi.json` answer without a token. Without one
  that opens, every other route answers 401 `{"detail": "Not
  authenticated"}`; to a token without the route's scope, 403
  `{"detail": "Forbidden"}`. A token is `rmm_` and 64 more characters, sent
  as `Authorization: Bearer`.
- `GET /api/tasks` lists the task definitions, a few kilobytes, and needs
  `tasks.run`; `GET /api/tasks/status` grows with every task run.
- A quick scan made the folder ROM 1. Renamed, a quick scan kept ROM 1 and
  changed its `fs_name`; removed, ROM 1 stayed with `missing_from_fs` true;
  put back under a third name, ROM 1 again, no longer missing.
- A scan of a slug the library has no folder for is queued, and finishes
  having scanned nothing.
- `search_term` matches fuzzily (`Probe`, `(USA)` and `Probe Game (U` all
  found the game), so a match is checked against `fs_name` exactly.
- A task RomM does not have answers 404.
- RomM's page for a ROM is `/rom/{id}`.
- From RomM's source at each release tag: client tokens arrived in 5.0.0;
  `POST /api/tasks/scan` in 5.4.0. `GET /api/roms` takes `platform_ids`,
  `search_term`, `limit`, `offset` and `with_files` from 5.0.0 on.
