# Changelog

Notable changes to Tangible, newest first. Versions follow semantic
versioning; during 0.x the API, the manifest schema and the database schema
may change between minor versions, and the release notes say how.

## Unreleased

### Added

- Requests (ADR 0021, the first part): anybody signed in, viewers
  included, may ask for an item on the new Requests page, describing it
  and ticking a box to attest, on every request, that they legally own it
  and want a digital image of it. The server refuses a request without the
  attestation and does not keep it. An operator or administrator approves
  or declines each request, saying why when declining; their own requests
  are approved as made. A viewer sees and withdraws only their own. Each
  request and decision is in the audit log. Nothing is searched for or
  downloaded yet: finding a release for an approved request comes later.
  Migration 0035 adds the requests table; the previous version neither
  reads nor writes it.
- Finding an item to ask for (ADR 0021): Ask for an item searches the
  loaded DATs, and MusicBrainz (music), TMDB (films and television, and
  IMDb IDs or addresses, which TMDB looks up) and IGDB (games) where an
  administrator has added them on the Integrations page, under Metadata
  providers. MusicBrainz needs no key; TMDB takes an API Read Access Token;
  IGDB a Twitch application's client ID and secret, kept sealed like every
  integration secret. A search sends a provider only the words searched
  for, at public addresses only, kept to each provider's rate limit; a
  provider that fails is named and the rest still answer. Picking what was
  found fills in the request. Migration 0036 adds the provider kinds.
- A "Buy me a coffee" link at the foot of the web UI's sidebar, under the
  source code link, and a button in the README, for anyone who wants to
  support Tangible's development. The link is plain text: the page loads
  nothing from elsewhere, and nothing is fetched unless it is followed.

## 0.2.1 (2026-11-12)

A security release: a review of 0.2.0 found the faults under "Security",
and this release fixes them. Upgrade if you import archives, export to
RomM, use watched folders, make CHDs or run burn workers.

### Upgrading from 0.2.0

The usual upgrade, as deploy/README.md ("Upgrading") says: take a backup,
change the image to this release's digest, pull, migrate, start. The one
migration, 0034, only adds a column. Upgrade every burn worker as well,
since some of the fixes are in the worker; a 0.2.0 worker still works with
a 0.2.1 server, without them. If you built the decryption image, build it
again from this release.

### Fixed

- Between 900 and 1199 pixels wide the web UI's sidebar showed its icons
  only, with no way to see the pages' names but hovering over each. A
  button at its foot now expands it, and collapses it again, at any width
  from 900 pixels up; the choice is remembered in the browser. Below 900
  pixels it is a drawer, as before.

### Security

- The RomM export could be made to delete files outside RomM's folder,
  library originals included. Each exported game's folder holds a marker
  listing the files Tangible wrote, and re-exporting or taking the game out
  deleted what the marker named; the marker sits where RomM can write, and
  its names were not checked, so an absolute path or `../` reached anything
  the server could delete. A marker is now taken as Tangible's only if every
  name is a plain file name in that folder; otherwise the folder counts as
  one Tangible did not make and nothing in it is touched. The marker is read
  without following a link, only as a regular file of at most 1 MiB, and
  only regular files are deleted.
- A crafted tar could run the server out of memory. Its unpacking guard
  counted each entry's size from the entry's header, while the tar reader
  takes a PAX `size` record over it; a PAX size of 0 put the guard out of
  step, and a GNU long-name header past the guard's 1 MiB cap was read
  whole, however large. The guard now follows PAX sizes as the reader does.
  The same fix lets a tar holding a file over 8 GiB, whose size only a PAX
  record can give, be imported: before, it was refused as damaged.
- A crafted 7z could run the server out of memory. The 7z reader makes room
  for every file a header declares before Tangible counts them, and a header
  of up to 64 MiB was let through, enough to declare tens of millions; a 7z
  whose start header was zeroed skipped the check altogether, and the
  reader went looking for a header of its own. A 7z's header is now capped
  at 4 MiB, as stored or as decoded, which still fits an import's 10,000
  entries with paths of 180 characters, and a 7z with a missing start
  header, or one that does not match its checksum, is refused as damaged.
- An import that stopped the server, such as one running it out of memory,
  was taken up again on every start, so it stopped the server every time.
  An import the server stops during three times now fails for good with
  `IMPORT_INTERRUPTED`, and the queue moves on; retrying it runs it again.
  A retry's delay, or a wait for a download client, does not count. The
  database gains a column for the count (migration 0034, additive).
- Any enrolled burn worker could send events for another worker's burn
  attempt: add to or bury its history, or report an attempt that had not
  begun writing as writing, which then needed an operator to settle. Only
  the attempt's own worker may now; any other gets `404 NOT_FOUND`, as
  does an attempt that does not exist, which before was a server error.
- A burned disc could go unrecorded. A burn worker that restarts tells the
  server the stage it had reached, and the server did not understand two
  of the worker's stage names, `finalizing` and `completing`, which come
  after a write; it took them as nothing written, and if it had not heard
  the write begin, it told the worker to discard its record of the disc and
  failed the attempt as never written, so a retry burned another. Those
  stages, and any name the server does not know, now count as possibly
  written, and the attempt is held for a person. And a worker whose report
  of how a burn ended did not reach the server went on to the next burn,
  whose record replaced the first's; it now asks again, with a pause up to
  a minute, until the server takes the report, and if the server refuses
  it, reconciles before taking more work.
- A burn worker trusted two things in what the server sent. It named a
  folder after a rip's identifier and emptied it first, so a server, or
  anything between it and the worker over plain HTTP, sending `../` or an
  absolute path could have the worker delete files outside its rips
  folder, its credential included; a rip identifier that is not a UUID is
  now refused. And it fetched a burn's manifest from a path the server
  sent, pasted onto the server's address, so a path beginning `@` sent the
  request, with the worker's credential, to another host; the worker now
  fetches the manifest by the artifact's identifier from its own server.
- Making a CHD could read files outside the image. An imported CUE or TOC
  naming `../../somewhere/file` has the name reduced to `file` to find it
  among the image's files, but chdman was given the sheet as it was and
  followed the name out, and a comment longer than chdman reads in one go
  could hide a file line from Tangible altogether; a derivation that then
  failed its check said how the file it reached differed. chdman now reads
  a CUE that Tangible writes, naming the image's own files; a TOC must name
  only those; and either is read again as chdman reads it, so a sheet that
  would have chdman open anything else, or that has a line it could split,
  is refused with `DERIVATION_SHEET_REFUSED`. The imported sheet is left
  as it is.
- A watched folder's files were checked, then used by their paths, so
  whatever writes the folder, such as a download client, could swap a
  folder on the path for a link in between: an import then copied a file
  from anywhere the server could read, its secret key included, into the
  library, and removing a download's source after import could delete a
  library object, or an empty folder, outside the watched folder. Files
  are now read, and files and folders deleted, through the watched folder
  opened as a directory and each name beneath it opened in turn without
  following a link, and a file is deleted only while its name still names
  the file that was hashed.

## 0.2.0 (2026-11-12)

Tangible reads discs as well as writing them, fetches what it imports, and
looks after what it keeps. The highlights, each detailed below:

- Ripping CDs and DVDs in a worker's drive, and encrypted DVDs with an
  opt-in worker image the operator builds (deploy/README.md, "Ripping
  encrypted discs").
- Importing from a URL, in resumable parts, and from qBittorrent, SABnzbd,
  NZBGet and S3-compatible storage, optionally removing the source once the
  library's copy has been checked.
- CloneCD, Alcohol 120% (MDS/MDF) and CHD originals, and archives unpacked
  on import; CUE/BIN, ISO and CHD made from them.
- Integrity checks, scheduled scrubs and a garbage collection plan;
  preservation hashes and DAT matching; RomM's API, so exported games are
  scanned and found.
- Signing in through an OpenID Connect provider, an audit log, worker
  management with credential rotation, deletion as tombstones, Prometheus
  metrics, and a live event stream.
- A web UI redesigned throughout, in a light and a dark theme.
- The image is built on Debian 13.

### Upgrading from 0.1.0

1. Take a backup of the database and the library, as deploy/README.md
   ("Backups") says. This release adds 25 database migrations (0009 to
   0033), and they only move forward: 0.1.0 cannot run on the migrated
   database, so going back means restoring that backup.
2. Compare your Compose files and `.env` with this release's
   `deploy/compose.yaml`, `deploy/compose.hardware.yaml` and
   `deploy/.env.example`, and take what you need. Every new setting is off
   unless set, except preservation hashing (below), and the server now
   has a minute to stop (`stop_grace_period`).
3. Point the server and every worker at the new image, by digest, then
   `docker compose pull`, `docker compose run --rm server migrate`, and
   `docker compose up -d`, as deploy/README.md ("Upgrading") says. Workers
   run the same image; upgrade them with the server.
4. A worker given an `http://` server URL across the network now refuses to
   start. Use `https://`, or set `TANGIBLE_WORKER_PLAIN_HTTP=true` on that
   worker (under Changed).
5. If you built the opt-in decryption image, build it again from this
   release: its stages are on Debian 13 now.

After the upgrade the server computes preservation hashes (CRC32, MD5 and
SHA-1) of every artifact already in the library, in the background, which
reads each one once. Set `TANGIBLE_PRESERVATION_HASHES=false` to leave it
off. And 0.1.0 kept a second copy of every import's bytes in
`staging/imports/`: those of imports shown as complete can be deleted
(under Fixed).

### Added

- What a drive is doing, in the drive views (`GET /api/v1/drives` and
  `GET /api/v1/drives/{drive_id}`): `activity`, the open burn, rip or
  erasure on it, with its state (a burn's job state, so waiting for a disc
  shows), the burn's disc title and number, the latest progress, the disc
  the worker inspected, and the burned artifact's shape; `null` when nothing
  is open. For the web UI's drive bay.
- Library items carry `disc_shape`, the disc their manifest describes: its
  media family, its length in sectors and its tracks (session, sectors,
  audio or not), for drawing it. And the library can be searched by name:
  `GET /api/v1/artifacts?name=` keeps artifacts one of whose files has a
  name containing the text, in any case, as received or as stored.
- Archives are unpacked on import: when the one file an import holds is a
  ZIP, a 7z or a tar (plain, gzip or xz), recognised by its bytes, its files
  are imported in its place, so a CUE/BIN in a `.7z` arrives as the CUE/BIN.
  The archive is not kept; the manifest records it as `origin.container`
  (format, SHA-256, size, files, notes) and the artifact detail as
  `unpacked_from`. Refused, with a stable failure code: encrypted archives
  (`ARCHIVE_ENCRYPTED`), names that are not plain relative paths, links,
  devices, sparse files and names twice (`ARCHIVE_UNSAFE_ENTRY`), more than
  10,000 entries, more than `TANGIBLE_MAX_UPLOAD_BYTES` or a thousand times
  the archive's size unpacked, a 7z dictionary over 1 GiB, or an index the
  decoders would hold whole in memory (a ZIP's central directory or a 7z
  header over 64 MiB, a tar extension header over 1 MiB), checked from the
  bytes before it is read (`ARCHIVE_TOO_LARGE`), unreadable data
  (`ARCHIVE_CORRUPT`), unknown methods (`ARCHIVE_UNSUPPORTED`). macOS's
  `__MACOSX` and `.DS_Store` are left out; an archive inside is imported as
  a file. An interrupted unpack starts again cleanly, from state kept in a
  staging area of Tangible's own (`<staging>/unpacks/`) that no source
  writes to. No RAR. Pure Rust decoders: `zip`, `sevenz-rust2`, `tar`,
  `flate2`, `lzma-rust2`. An artifact's page says what archive its files
  came in, its size, digest and notes, and that the archive itself is not
  kept.
- Artifacts can be deleted, as tombstones. An administrator (new permission
  `delete_artifacts`) asks with
  `POST /api/v1/artifacts/{id}/deletion-requests` (an original needs
  `confirm_original`); the artifact is pending for
  `TANGIBLE_DELETION_GRACE_DAYS` (7 by default), when `DELETE` on the same
  path keeps it, and then its bytes are removed and its record kept:
  components, hashes, manifest, lineage, burns and discs. Refused with a
  retention lock (`ARTIFACT_RETAINED`) or while a burn, derivation, check or
  source removal of it is under way (`ARTIFACT_IN_USE`); while pending,
  burns and derivations of it are refused and the RomM export leaves it;
  once deleted, its content and checks are refused (`ARTIFACT_DELETED`). A
  stored file is removed only when no artifact still has it, under a lock
  every import and derivation holds until it has registered what it stored.
  The library list leaves deleted artifacts out (`?deleted=true` lists
  them); every artifact view carries `deletion`, and `origin`. Scrubs, the
  GC plan, restore checks, the dashboard and metrics leave them out.
  Migration 0033. On an artifact's page, "Delete this artifact" says what
  happens and, for an original, asks to be told it may be the only copy; a
  pending artifact says when it goes and offers "Keep"; a deleted one shows
  its record with nothing to download, check or burn. The Library marks
  pending deletions and can list only deleted artifacts.
- `GET /api/v1/events`, a server-sent event stream: what changes in imports,
  burns and their progress, rips, erasures, derivations, workers and the
  library, as it commits, heard from the database itself (migration 0031).
  Topics can be chosen; a client reconnecting with `Last-Event-ID` is sent
  what it missed, or `stream.reset` to reconcile. Streams end after ten
  minutes and at shutdown; one account may have 16 open, the server 256.
  Shutdown waits at most 30 seconds for requests still under way. The burn
  queue and each burn, the rips and each rip, the imports and the Workers
  page now follow it, refreshing as things change and reconciling every
  minute; where a browser has no EventSource they poll as before.
- Worker credentials are rotated. A worker's heartbeat says
  `credential_rotation_required` when an administrator asked
  (`POST /api/v1/workers/{id}/rotate-credential`) or the credential is older
  than `TANGIBLE_WORKER_CREDENTIAL_MAX_AGE_DAYS` (90 by default, `0` for
  never); between jobs the worker asks for a new one
  (`POST /api/v1/workers/{id}/credential-rotations`), saves it and uses it.
  The old credential works until the new one is first presented, then never
  again, so a lost answer or a restart halfway never locks a worker out.
  Issuing, rotating and asking are audited. Workers show when their
  credential was issued and whether a rotation is due. The worker's identity
  file is now written readable by its user alone and flushed to disk before
  it replaces the old one. The Workers page shows each credential's age, or
  that a rotation is asked for or due, and has a "Rotate credential" button
  for administrators.
- `GET /api/v1/drives/{id}` answers one drive as the drive list does.
- The library can be filtered: `GET /api/v1/artifacts` takes `format`,
  `origin`, `validation_state`, `integrity_state`, `linked` (`false` for
  artifacts linked to no disc) and `created_after`/`created_before`,
  answered from the catalogue rather than by reading every manifest. An
  unfiltered list is unchanged. The Library page has a filter bar for
  format, where an artifact came from, validation, stored bytes and whether
  it is linked to a disc.
- `GET /api/v1/dashboard`, for anyone signed in, answers in one request what
  needs doing and what is under way: imports under way and failed in the
  last week; burn jobs queued, waiting for a disc, under way and needing
  attention; workers online, silent, draining, pending and incompatible;
  artifacts damaged and never checked; objects stored and the library
  volume's free space; and the five latest ended burns and jobs needing
  attention, by title and disc number. The home page shows it as cards, each
  in words and linking to its page, headed "Needs somebody" when a burn
  needs attention or a disc, a worker has gone silent, an artifact is
  damaged or an import failed this week.
- The catalog can be edited: `PATCH` on a title, edition, disc set or disc
  changes the fields given, `null` clearing one that may be empty. Every
  catalog row now has a `version`, its `ETag`; an edit sent with `If-Match`
  naming a version somebody has since changed is refused with
  `409 RESOURCE_VERSION_CONFLICT` rather than overwriting their change. A
  renamed title's sort title follows the name unless it was set on its own.
  Disc numbers stay unique within a set. Edits need the `catalog` permission
  and are audited. `GET /api/v1/editions/{id}` and `/disc-sets/{id}` are
  new. Database migration 0029 adds the version columns, additively. On the
  Catalog page, "Change this title", "Change this edition", "Change this
  set" and "Change" on each disc open a form that saves only what changed,
  and says so when somebody else changed it first.
- The audit log can be read: `GET /api/v1/audit-events`, newest first and
  paged, filtered by action (or a kind of action, such as `worker.`), actor,
  target and outcome, and `GET /api/v1/audit-events/{id}`. A new permission,
  `read_audit`, held by administrators, guards both. The events have been
  written all along; until now only the database could show them. The web
  UI's Audit log page shows them in words, filtered by kind, who and what,
  with older events on request.
- Workers can be managed: `GET /api/v1/workers` and
  `GET /api/v1/workers/{id}` list them with their status and drives, and an
  administrator can drain one (it finishes what it holds and is given no new
  burns, rips or erasures; its heartbeat says `drain`), enable it again, or
  revoke it, which refuses its credential from its next request for good.
  Each change is audited. Until now nothing set a worker draining or revoked
  short of editing the database. The Workers page lists them, with Drain,
  Enable and Revoke (which asks first and says what it does).
- CHD images imported as originals are recognised: the header and the
  metadata chain are read with a bounded, fuzzed reader checked against
  chdman 0.251's own reading of CHDs it made, never the compressed data.
  The CHD says which disc it holds (a CD, a GD-ROM or a DVD; a hard disk or
  laserdisc is said not to be one), and its shape, SHA-1s and CD tracks
  are recorded as evidence. A version other than 5, a delta that needs its
  parent, and metadata that does not add up are imported and say so. An
  imported CHD is not sent to RomM as it is.
- An imported CHD can be extracted by chdman into what burns: a CD CHD into
  a CUE/BIN (`cue_bin_from_chd`), a DVD CHD into its ISO (`iso_from_chd`).
  chdman verifies the CHD first; the CUE/BIN is made into a CHD again and
  kept only if that has the original's data SHA-1, and the ISO only if its
  own SHA-1 is the CHD's data SHA-1. Either is recorded as structurally
  equivalent, and burns, makes a checked CHD and goes to RomM. A delta, or
  a CD CHD that keeps subcode, is refused saying why. A CHD Tangible made is
  not offered extraction: what it was made from is the CUE/BIN or ISO.
  Database migration 0028 widens the derivation jobs' transformation
  constraint, additively.
- The web UI offers "Make a CUE/BIN" or "Make an ISO" on an imported CHD's
  page, saying chdman checks it and that it is a full-size copy.
- Alcohol 120% images (MDS/MDF) are recognised and imported as one
  artifact: the binary `.mds` is read with a bounded, fuzzed parser written
  from libmirage's reading of the format and checked against libmirage
  3.2.10. Version 1 descriptors only; another version is imported whole,
  saying so. The data file is found by the name the descriptor gives
  (`*.mdf` is its own name), in its folder, ignoring case with a warning. A
  single-session CD without subchannel is laid out as a CUE sheet's tracks
  are; its pregaps are not in the MDF, so a track after the first does not
  hash as a DAT that includes its pregap lists it. A DVD image whose one
  MDF is an ISO is described as one. An image with subchannel after every
  sector, several sessions or a DVD in several files is imported whole,
  saying why. An MDS image is refused for burning as it is.
- An MDS image can be made into what burns and plays, by Tangible itself
  with no derivation engine: a CD into a CUE/BIN (`cue_bin_from_mds`),
  its BIN the MDF's own stored bytes, or, when the MDF interleaves
  subchannel, a new BIN of each sector without it; a DVD whose one MDF is
  an ISO into that ISO (`iso_from_mds`), the same stored bytes, read back as
  an ISO 9660 volume. Both are lossy, saying what they do not carry. The
  CUE sheet writer now writes a generated pregap as PREGAP. A disc linked
  only to an MDS image says what to make of it before RomM gets it.
  Database migration 0027 widens the derivation jobs' transformation
  constraint, additively.
- The web UI names MDS images ("Alcohol 120% (MDS/MDF)"), offers "Make a
  CUE/BIN" or "Make an ISO" on an MDS image's page, saying what each leaves
  out, and in place of "Create burn job" says it is not burned as it is and
  what to burn instead.
- Signing in through an OpenID Connect provider, such as VoidAuth (ADR
  0018): authorization code flow with PKCE, ID tokens checked for
  signature, issuer, audience, expiry and nonce, and each sign-in bound to
  the browser that started it. Roles come from the provider's groups,
  mapped by `TANGIBLE_OIDC_ADMIN_GROUPS`, `_OPERATOR_GROUPS` and
  `_VIEWER_GROUPS` and read at every sign-in; someone in no mapped group is
  refused. Accounts are the provider's person (issuer and subject), never a
  name. Configured with `TANGIBLE_OIDC_ISSUER`, `_CLIENT_ID` and
  `_CLIENT_SECRET_FILE`. `TANGIBLE_PASSWORD_SIGN_IN=setup-only` leaves
  passwords only to the administrator setup made. The sign-in page offers
  the provider and says why a sign-in failed; the Accounts page says how
  each account signs in; Diagnostics reports it. Database migration 0026
  adds two tables and makes the password optional for provider accounts.
- Diagnostics (the page and `tangible doctor --report`) says how far
  preservation hashing has got, warns of artifacts left failed, counts the
  DATs loaded, and says whether metrics are served and behind a token,
  warning when they are served without one.
- `tangible manifest show <artifact>` prints an artifact's stored manifest;
  `tangible manifest validate <file>` (or `-`) checks a manifest the way the
  server reads one and says whether it is in the canonical form the server
  writes. The subcommand existed and did nothing until now.
- Audit events for loading and removing a DAT (`dat.loaded`, with its
  name, games, files and whether it replaced one; `dat.removed`), for
  asking for an artifact's hashes again (`artifact.rehash_requested`), and
  for `restore-check` cataloguing an artifact again
  (`artifact.recatalogued`, by the system).
- Worker enrollment is rate limited, as sign-in is: after five rejected
  tokens under one worker name in fifteen minutes, or sixty in all, further
  rejected tokens are answered 429 `RATE_LIMITED` with `Retry-After` until
  the window passes, without a log line each. A valid token always enrolls,
  so bad tokens cannot lock a real worker out.
- Every command warns, at start, of each `TANGIBLE_` environment variable
  that is set and that nothing reads, which is most often a mistyped
  setting otherwise silently ignored. Only the name is logged, never the
  value.
- `tangible restore-check`, for after restoring a backup: whether every
  object and manifest the catalogue names is in the library at the size
  recorded, whether the library holds artifacts the catalogue lacks
  (imported after the database was dumped), and what work the catalogue
  records as unfinished, in seconds, without reading the bytes.
  `--register-missing` catalogues those artifacts again from their
  manifests once their objects are all there. It exits non-zero when
  anything needs attention.
- Prometheus metrics, on a listener of their own, off unless
  `TANGIBLE_METRICS_BIND` is set: artifacts and stored bytes, jobs of every
  kind by state and the age of the oldest waiting, burn attempts and their
  durations, workers and drives, integrity, preservation hashes, DAT
  matches and integrations, all read from the catalogue at each scrape and
  reused for five seconds. `TANGIBLE_METRICS_TOKEN_FILE` makes a scrape
  carry a bearer token. `deploy/grafana/tangible.json` is a dashboard
  built on them.
- The web UI shows what identifies an artifact: its page has an
  Identification section naming each game of a loaded DAT it matches, in
  words ("Matches Game (USA), all 2 of its files", "Partly matches...: 1 of
  2 files"), which track or file matched each of the game's files, and its
  preservation hashes folded away, with "Hash again" for operators. The
  library list has an "Identified as" column. Administrators have a DATs
  page to load a DAT from a file, see what each identifies, and remove one
  after confirming.
- DAT matching: an administrator loads a Logiqx XML DAT, such as Redump's
  or No-Intro's (`POST /api/v1/dats`, the XML as the body, up to 64 MiB;
  `GET /api/v1/dats`; `DELETE /api/v1/dats/{id}`; new permission
  `manage_dats`), and every artifact is matched against it by SHA-1 and
  size: its CD tracks, or its image files. A game whose every file (its CUE
  aside) matched, with nothing left over, is an exact match; some, a
  partial one, as an audio CD read without offset correction is. An
  artifact's identification lists its matches and which track or file
  matched each file; the library list shows each artifact's best match.
  Matching renames nothing. The DAT parser is bounded, fuzzed, and expands
  no entity beyond XML's own; a file that is not a DAT, or one cut short,
  is refused with `DAT_INVALID`. A DAT with the name of one already loaded replaces it.
  Tangible never downloads DATs. Database migration 0025 adds tables,
  additively.
- Preservation hashes: the CRC32, MD5 and SHA-1 of every artifact's files,
  and of every CD track's bytes as Redump cuts them (pregap in the file
  included), computed in the background by reading the stored bytes once,
  for new imports and derivatives as they arrive and for the library
  already there. `GET /api/v1/artifacts/{id}/identification` shows them;
  `POST .../identification/rehash` asks again. An unreadable artifact is
  tried three times, an hour apart, then left failed, saying why.
  `TANGIBLE_PRESERVATION_HASHES` (on by default) turns it off. Database
  migration 0024 adds a table and columns, additively; every existing
  artifact starts pending.
- CloneCD images (CCD/IMG/SUB) are recognised and imported as one artifact:
  the `.ccd` is read with a bounded parser (no published specification
  exists; it follows CloneCD's open-source readers), the IMG and SUB are
  found by its name, each track's mode is read from the image itself, and a
  single-session image's tracks are laid out as a CUE sheet's are. The SUB
  is kept as a `subchannel` component and the manifest says how it is
  stored (`pw96_linear`). A multi-session or scrambled image is imported
  whole without a layout. An image that keeps a subchannel is refused for
  burning, since no engine writes one; and a disc linked only to a CloneCD
  image says what to make of it before RomM gets it. Fuzzed, and checked
  against GNU ccd2cue.
- A CloneCD image can be made into a CUE/BIN (`cue_bin_from_ccd`, asked
  for like a CHD, and suggested for a CloneCD image): made by Tangible
  itself, with no derivation engine needed, its sheet written from the
  image's tracks and read back to check it gives the same tracks, and its
  BIN the IMG's own stored bytes, so nothing is copied. Its lineage says it
  is lossy and that the subchannel and raw TOC are not carried. It burns,
  makes CHDs and exports like any CUE/BIN. Derivation jobs now record the
  tool `tangible` for it; database migration 0023 widens their
  transformation constraint, additively. A server without chdman still runs
  derivations, offering only what it can make.
- The web UI names CloneCD images ("CloneCD (CCD/IMG/SUB)"), offers "Make a
  CUE/BIN" on a CloneCD image's page, saying it drops the subchannel, and in
  place of "Create burn job" on an image that keeps a subchannel says why it
  is not burned as it is and what to burn instead.
- Integrity checks: an artifact's stored bytes can be read back and
  compared with the digests recorded for them
  (`POST /api/v1/artifacts/{id}/validate`, operators), and so can the whole
  library (`POST /api/v1/integrity-checks`, administrators, with a new
  `maintain_storage` permission). Checks run in the background, one at a
  time, show their progress, resume after a restart, and record what they
  find: a missing, unreadable or changed object, or a manifest that no
  longer agrees with the catalogue. Artifacts gain an integrity state
  (unchecked, intact, damaged). Nothing is repaired; damage is audited.
  Database migration 0018 adds a table and two columns; it is additive.
- `tangible scrub` checks the library, or one artifact with `--artifact`,
  from the command line, with or without the server running, and exits
  non-zero when it finds anything. `TANGIBLE_SCRUB_INTERVAL_DAYS` has the
  server queue a full scrub that many days after the last ends; it is off
  by default. Diagnostics shows how many artifacts are damaged (a Problem)
  and when the last full scrub ended and what it found. deploy/README.md,
  "Checking stored bytes", says how to restore a damaged file.
- `tangible gc` and `GET /api/v1/storage/gc-plan` (administrators) plan
  garbage collection, a dry run: stored files nothing refers to and older
  than a grace period (7 days by default), with what removing them would
  free, plus leftover partial files, entries that are not stored files, and
  recorded files that are missing. Nothing is removed.
- The web UI shows integrity: an artifact's page says whether its stored
  files were last found intact or damaged, and "Verify now" (operators)
  checks them and follows the check; the library list marks damaged
  artifacts. A Storage page for administrators runs and withdraws full
  scrubs, lists every check with what it found, and shows the collection
  plan on request.
- RomM's API as an integration (kind `romm_api`, administrators): RomM's
  address and a client token (RomM 5.0 and later) with `platforms.read`,
  `roms.read` and `tasks.run`, sealed like a download client's password.
  Trying it reads RomM's version and its own OpenAPI document and says,
  check by check, whether exported games can be found, whether RomM can be
  asked to scan (5.4 and later), and whether its platforms can be checked.
  Tangible never changes anything in RomM. There is one, for the one
  export. Database migration 0019 widens the integration kinds; it is
  additive. `POST /integrations/{id}/test` answers a new `checks` list,
  empty for other kinds.
- deploy/README.md now tells RomM to leave the export's marker file out of
  each game (`exclude.roms.multi_file.parts.names` in RomM's
  `config.yml`). Without it RomM, which counts every file in a game's
  folder, listed `.tangible-export.json` as one of the game's files, made a
  one-image game a several-file one, and sent the marker with every
  download. Trying the RomM API integration checks it ("Export marker").
- RomM is asked to scan what the export changed. Each folder written,
  renamed or taken out leaves its platform waiting for a scan; once RomM's
  API is connected (with `scan_after_export`, on by default, and RomM 5.4
  or later), the server asks for one quick scan of every waiting platform
  and follows it to its end, or for 30 minutes. A scan RomM refuses (one
  already running, or no scan worker listening) or that fails is asked for
  again, after a minute and then less often, up to hourly. Every scan asked
  for is recorded and audited. `GET /api/v1/romm` gains `api`: the
  integration's health, the platforms waiting, and the last scan; an
  edition's RomM status gains `scan_owed` and the last scan of its
  platform. Diagnostics gains a "RomM API" check. Database migration 0020
  adds two tables; it is additive.
- Exported games are found in RomM. After each scan, and hourly, the
  server looks each export up among RomM's ROMs by its platform folder and
  folder name, and records RomM's id for it and whether RomM's files agree
  with what was written (names, and sizes). An edition's RomM status gains
  `in_romm`: `unseen`, `linked`, `missing` or `differs`, with what
  differs, RomM's id, and a link to the game's page in RomM. A removed
  export is followed until RomM marks it missing. Each platform in
  `GET /api/v1/romm` says whether the connected RomM knows it. Removing
  the RomM API integration, or pointing it at another address, forgets
  the links. Database migration 0021 adds four columns to the export's
  status and a size to export receipts; it is additive.
- The web UI shows RomM's API. The Integrations page adds and edits it
  (RomM's address for the server and for your browser, the client token,
  and whether to ask for scans) and lists, check by check, what trying it
  found. An edition's RomM section says what RomM made of the game, in
  words (RomM has it, has not seen it yet, marks it missing, sees different
  files, and what differs), links to the game's page in RomM, says when a
  scan is awaited and how the last one went, and warns of a platform the
  connected RomM does not know.
- A source can be removed once the library has it. qBittorrent and S3
  integrations gain `after_import`: `keep` (the default) or `remove`. With
  `remove`, each import completed from then on is followed by an integrity
  check of its artifact, and only when that finds nothing wrong is the
  source removed: the torrent with its files, if it is still in Tangible's
  category at the path imported; the object version imported, if its size
  and ETag are still the ones imported (a delete conditional on `If-Match`
  where the bucket keeps no versions). Anything else is left, saying why;
  an unreachable client is tried again. Each import's view gains
  `source_removal`. Each removal is audited as `integration.source_deleted`.
  Database migration 0022 adds a table and a column; it is additive.
- SABnzbd and NZBGet integrations gain `after_import` too. Their APIs
  cannot delete a completed download's files, so with `remove` Tangible
  deletes them itself, once the library's copy has passed its check: only in
  a watched root listed in the new `TANGIBLE_REMOVABLE_ROOTS` (mounted
  read-write; the server refuses to start if one listed is not watched, and
  an integration is refused `remove` on a root not listed), only the files
  whose SHA-256 and size are the artifact's, checked just before each
  delete, and then the folders left empty. Anything else stays and is
  counted. SABnzbd is then asked to forget the job (it archives it), NZBGet
  to hide it. Diagnostics gains a "Removable roots" check.
- The web UI offers it: on the Integrations page, each download client and
  S3 integration has "After an import is in the library", keep or remove,
  saying what goes and when; a SABnzbd or NZBGet offers removing only on a
  root Tangible may delete in, and says why not otherwise. The Imports page
  says where each removal stands. `GET /api/v1/import-sources` gains
  `removable_roots` (ids only). Saving an integration from the page now
  keeps its `after_import` rather than turning removal off.

- Groundwork for issue reports: text is scrubbed of credentials, paths,
  serial numbers, library file names and LAN addresses before anyone can
  send it, and two settings say where reports go
  (`TANGIBLE_REPORT_ISSUES_URL`, empty to turn them off) and whether
  addresses are scrubbed. The server names its release and commit in them.
- Drives are recorded with their vendor, model and firmware, from what the
  drive itself reports, and with how they are attached (USB or SATA), read
  from the host's device tree. The Workers page shows both.
- A burn attempt records what preflight found in the tray: the media
  profile, whether it was blank, its capacity and the disc's maker
  (`medium` on `GET /api/v1/burn-attempts/{id}`).
- A burn attempt can be prepared as an issue report:
  `GET /api/v1/burn-attempts/{id}/report` fills in the burn form for a
  problem, or a new "Drive and media that worked" form for a disc that
  burned and checked, scrubbed and ready to review.
- A burn's page offers both reports, and shows everything before the issue
  form opens in a new tab. It also now shows what the worker said when an
  attempt failed, each event's message, and the disc preflight found.
- Every error the web UI shows now carries its request id, so it can be
  found in the server's log. An error that means Tangible itself went wrong
  offers "Report this…", which prepares it as a scrubbed bug report for
  review (`POST /api/v1/error-reports`).
- `tangible doctor --report` prints a diagnostics checklist safe to share:
  database and schema, storage room and writability, tools, workers and
  drives, recent failure codes and the settings that matter for support,
  with a link to a prefilled bug report. `--json` for scripts. Plain
  `doctor` is still the quick healthcheck.
- An image Tangible did not recognise, or read with doubts or warnings,
  offers "Ask for support…" on its page: a format support request that
  describes its structure (component sizes and extensions, the first bytes
  of binary files, known signatures at known offsets) with every name
  replaced (`GET /api/v1/artifacts/{id}/format-report`, new issue form
  `format_support.yml`).
- CDs can be ripped into the library: an operator asks for a rip of a
  drive's disc (`POST /api/v1/drives/{id}/rips`), the drive's worker reads
  it with `cdrdao read-cd`, sends the table of contents and data, and they
  are imported as a TOC/BIN artifact whose origin says it was dumped from a
  disc. A rip can be withdrawn until its import starts. Blu-rays and blank
  discs are refused. A drive now does one thing at a time across burns,
  erasures and rips. The web UI's Rips page asks for a rip and follows it to
  the library, with progress as the disc reads.
- DVD data discs can be ripped too: the worker reads the disc sector by
  sector from the drive itself into an image, retrying a sector that fails
  and stopping at one that will not read, and it is imported as an ISO. An
  encrypted (CSS) DVD is recognised while it reads and refused, keeping
  nothing, including on drives that will not read its video at all without
  authentication. A disc the system has not caught up with since a burn is
  refused with a request to reload it. Blu-rays are still refused.
- An opt-in worker image for ripping encrypted discs, built by the operator
  and never published (`deploy/decryption/Dockerfile`, with
  `deploy/compose.decryption.yaml` to run a worker from it): VideoLAN's
  libdvdcss from its source release, pinned by checksum, and Debian's
  `dvdbackup`, `libaacs0` and `libbdplus0`. A worker started from it says so
  in its log and reports the library and tool versions to the server.
- A rip can ask to decrypt an encrypted disc, and choose its original: the
  disc as it is with a decrypted copy beside it (the default), or only a
  decrypted image (`decrypt` and `keep` on `POST /api/v1/drives/{id}/rips`).
  Off unless an administrator sets `TANGIBLE_ALLOW_DECRYPTION=true`, and
  only on a drive whose worker can; otherwise the request is refused with
  `DECRYPTION_OFF` or `ENCRYPTED_NO_DECRYPTION`, and a queued rip that can
  no longer decrypt when its worker takes it is refused the same way. The
  drive list says which drives can (`can_decrypt_rips`), and diagnostics
  shows the setting.
- The server accepts what a decrypting rip sends: a decrypted `VIDEO_TS`
  tree as the original, or the encrypted disc image with a decrypted copy
  beside it, which is imported after the original as its derivative
  (`decrypt_css`, made from the disc). A ripped artifact's manifest now
  records how it was read (`origin.physical_dump`): the drive, the engine,
  read errors, the encryption found, and whether and how it was decrypted
  at acquisition, never a key.
  The release now checks its image contains none of these before signing
  it (`deploy/check-no-decryption.sh`).
- Workers decrypt CSS DVDs for a rip that asks. The drive is authenticated
  through dvdbackup and libdvdcss first; a disc that turns out not to be
  encrypted is read as it is. Keeping the encrypted original reads the disc
  whole, scrambled sectors as they are, then makes the decrypted copy while
  the disc is still in the drive; keeping a decrypted original makes only
  the copy. dvdbackup runs with fixed arguments, a cleared environment, a
  time limit and its key cache off, and nothing it prints that could carry
  key material is kept. A disc that will not decrypt fails as
  `DECRYPTION_FAILED`, keeping nothing.
- The opt-in worker image also installs redumper, from its GitHub release
  pinned by build and SHA-256 for amd64 and arm64, for CD rips with the
  drive's read offset corrected. It is not in the published image, and the
  release's check refuses it there, because it carries its own CSS and AACS
  code.
- A rip can choose the tool for a CD (`cd_engine` on
  `POST /api/v1/drives/{id}/rips`): redumper, the default where the drive's
  worker has it, or cdrdao, for a drive whose redumper rips go wrong.
  Asking for redumper where the worker lacks it is refused
  (`CD_ENGINE_UNAVAILABLE`), when asked and again when the worker takes
  the rip. The drive list says which drives have it
  (`can_rip_with_redumper`), and the web UI offers the choice there.
- A worker built from the opt-in image now rips CDs with redumper: the
  drive's read offset is corrected, so audio tracks can match AccurateRip
  and Redump, and the rip becomes a CUE/BIN artifact (`disc.cue` and one
  file per track). Sectors that do not read cleanly are read again up to
  20 times; a disc with sectors that still will not read fails rather than
  being kept with holes. The rip and the artifact's manifest record the
  read offset corrected (none when redumper does not know the drive), the
  disc's write offset, and whether redumper treated the drive as generic
  (`cd_read`). A rip that asks for cdrdao, and every worker without
  redumper, reads CDs with cdrdao as before. Database migration 0014 adds
  a column; it is additive.
- A rip's page and the artifact's page say whether a CD's read offset was
  corrected, and by how many samples, or why not. For a CD read by
  redumper they also show the disc's write offset, whether the drive was
  treated as generic (with a hint to ask for cdrdao if its rips go wrong),
  and how many samples were read again.
- Operators can import an object from an S3-compatible bucket an
  administrator configured (`kind` `s3`: endpoint, region, path style, the
  buckets allowed, and an access key id and secret), with
  `POST /api/v1/imports/s3` naming the integration, bucket and key. The
  object is looked at first and the import pinned to its version and size;
  the fetch resumes like a URL import. `GET /api/v1/import-sources` lists the
  S3 integrations and their buckets. Requests are signed with `rusty-s3`.
  Tested against SeaweedFS's S3 gateway, which CI now runs. The
  Integrations page sets it up and tests it, and the Imports page has
  "Import from S3".
- The Integrations page sets up SABnzbd and NZBGet as well as qBittorrent,
  and tests and edits all three; the Imports page sends an `.nzb` to a
  Usenet client as it sends a torrent to qBittorrent, and names their
  imports ("Downloading in SABnzbd").
- NZBGet too (`kind` `nzbget`, with a user and password): an NZB is
  appended to its category, which must exist, with no post-processing
  parameters, and imported when NZBGet reports success. Tested against
  NZBGet 25.3.
- Operators can hand an NZB they already have to a SABnzbd an administrator
  configured (`kind` `sabnzbd`, with an API key), through the same
  `POST /api/v1/integrations/{id}/jobs` (`{"nzb": "<base64>"}`). The
  category must exist in SABnzbd, which is checked before anything is
  added; no script is asked for; the same NZB submitted again is found
  rather than downloaded twice. The import waits while SABnzbd downloads and
  post-processes, and is imported through a watched root when it completes.
  The job response now names the job by `identity` (an infohash or an NZB's
  SHA-256) and the client's `client_job_id`, in place of `infohash`. Tested
  against SABnzbd 4.5.3.
- Operators can hand a torrent they already have, a `.torrent` file or a
  magnet link, to a qBittorrent an administrator configured
  (`POST /api/v1/integrations/{id}/jobs`). Tangible adds it to its category
  and nothing else, follows it by infohash, and imports it through a watched
  root when it finishes; it never searches, runs a script, or removes the
  torrent or its data. The import waits in `acquiring` with its progress
  while the torrent downloads, and says why while qBittorrent cannot be
  asked. `POST /api/v1/integrations/{id}/test` signs in and records the
  client's version. `GET /api/v1/import-sources` lists the clients operators
  may use. Database migration 0017 adds a table and a column; it is
  additive. Tested against qBittorrent 5.1.2. On the Imports page, "Send a
  torrent to a download client" takes a `.torrent` file or a magnet link,
  and a torrent's import reads "Downloading in qBittorrent" with its
  progress, or "Waiting for qBittorrent" and why. The Integrations page
  gains "Test", and shows the client's WebUI API version.
- Administrators can configure integrations through the API
  (`/api/v1/integrations`, a new `manage_integrations` permission held by
  administrators), starting with qBittorrent's settings: its address (a LAN
  address is fine), user, category and where its downloads are mounted.
  Passwords are write-only: sealed with a key the server reads from
  `TANGIBLE_SECRET_KEY_FILE` (`tangible secrets generate` makes one,
  `tangible secrets rotate` moves to a new one) and never answered back,
  logged or audited. Without a key the server runs and integrations cannot
  hold a password. Diagnostics says whether a key is loaded and opens every
  stored password. Database migration 0016 adds a table; it is additive.
  An Integrations page, for administrators, lists them and adds, edits or
  removes a qBittorrent.
- Large uploads are sent in parts (`/api/v1/uploads`), so a dropped
  connection costs one part rather than the whole file. The Imports page
  uses parts for files over 64 MiB, shows how much has been sent, and, if
  the connection drops or the page is closed, carries on from where it
  stopped when the same file is chosen again. Room for the whole file is
  checked first; an upload left for a day is removed. Database migration
  0015 adds two tables; it is additive.
- Operators can import a file from a URL, where an administrator turns it on
  (`TANGIBLE_URL_IMPORTS=true`): "Import from a URL" on the Imports page, or
  `POST /api/v1/imports/url`. The server fetches it into staging, picks up
  where it left off if the connection drops, checks an optional SHA-256 and
  length, and imports it like an upload. It reaches only public addresses
  over https unless the administrator allows plain http or lists private
  blocks (`TANGIBLE_URL_IMPORTS_ALLOW_HTTP`, `TANGIBLE_OUTBOUND_ALLOW`), and
  a proxy only when one is named (`TANGIBLE_OUTBOUND_PROXY`). Requests are
  audited, refused ones included, limited to 30 an hour per account, and the
  address is recorded without its query string. A new permission,
  `import_url`, goes with the operator role. Diagnostics shows the setting.
- A worker refuses a rip it has no room for before reading anything
  (`INSUFFICIENT_SPACE`, saying how much it needs and has), rather than
  failing when its disk fills an hour in. It counts what each way of
  reading writes: a redumper rip of a full CD needs about 2.1 GB, and a
  DVD kept with a decrypted copy twice its size. Workers now report their
  free space.
- A CUE sheet that gives a track the ISRC `000000000000`, as redumper does
  for a track without one, is read as having none, with no warning.
- The web UI asks about decryption on a drive that can decrypt: "Rip the
  disc in {drive}" opens a dialog with a "Decrypt an encrypted disc" box,
  the notice that whether ripping is lawful is the operator's to determine,
  and, once ticked, the choice of original. A rip's page shows the
  encryption found, which library and tool decrypted it, the choice made,
  and a link to the decrypted copy. An artifact's page says "Ripped from a
  disc" with the drive, the engine, read errors and the encryption, and
  states plainly when an image was decrypted when ripped and is not a
  faithful copy of the disc. For this, `GET /api/v1/artifacts/{id}` gains
  `ripped`, and a rip's `decryption` is now described in the OpenAPI
  document (`DecryptionView`) instead of as any object.
- DVD-Video `VIDEO_TS` folders are recognised: imported from a watched folder
  (the `VIDEO_TS` folder itself, or the disc's folder holding it), a folder
  whose `VIDEO_TS.IFO` really is a DVD-Video manager becomes a
  `video_ts_directory` artifact with a directory-tree topology. Missing title
  sets, backups or video, files that are not DVD-Video, odd sizes, names
  that only match ignoring case, and video still CSS-scrambled are reported
  as warnings. A VIDEO_TS folder cannot be burned, since it is a disc's
  files rather than an image of one, and a burn of one is refused at once.
- A CD burn that the drive refused while setting up, before writing
  anything, is tried once more if the disc is still blank; a drive on the
  bench did this once and burned on the next run.
- Administrators have a Diagnostics page with the same checklist, to copy,
  download, or prepare as an issue (`GET /api/v1/diagnostics` and
  `/diagnostics/report`, new permission `diagnose`).

### Changed

- The web UI is redesigned throughout, as an operations console in a light
  and a dark theme. The theme follows the system unless Light or Dark is
  picked at the foot of the sidebar, and the choice is remembered in the
  browser. The Inter and JetBrains Mono fonts are bundled and served by
  Tangible itself (OFL-1.1; listed in `THIRD_PARTY_NOTICES-web.txt`), so the
  Content Security Policy is unchanged.

  Every signed-in page sits in one shell: a sidebar with the pages in groups
  (Overview; Collection: Library, Catalog, Physical copies; Activity:
  Imports, Rips, Burns, with counts of what is under way; Administration),
  and the account, theme, sign-out and source link at its foot; a top bar
  with where you are, a command bar (Ctrl+K: any page, the three actions,
  artifacts by name) and a New menu; and a drive bay showing each drive's
  state, the burn or rip in it and its progress, folded while every drive is
  idle. Below 1200 pixels the sidebar is icons only; below 900, a drawer.
  "System" is now "Overview" and "Discs" "Physical copies". One event stream
  per browser tab keeps it all current.

  Discs are drawn from their own data, to scale: an artifact's tracks in its
  library row and on its Topology tab (audio tracks set apart as on a
  pressed CD), a burn's written area growing outward as the worker reports,
  a rip's read area, an empty tray.

  Every state is a status mark, a shape and words, never colour alone.
  Destructive actions ask in place, under what they are about, and take the
  keyboard to the question. Pages are built from one set of components
  (buttons, fields, tables that scroll within themselves, panels, tabs,
  callouts, empty states), each checked with axe-core in its tests, and a
  test checks that every text colour meets WCAG 2.2 AA contrast in both
  themes. Signing in and setting up are a card centred under Tangible's
  mark, the identity provider's button first when one is set up.
- The image is built on Debian 13 (trixie), moving from Debian 12
  (bookworm), now oldstable, and every tool in it moves too: chdman 0.251
  to 0.276, which makes DVD CHDs (a DVD's ISO gets one, made with
  `createdvd` and checked by extracting it again byte for byte), cdrdao
  1.2.4 to 1.2.5, xorriso 1.5.4 to 1.5.6, glibc 2.36 to 2.41. chdman 0.276
  still misreads a cdrdao TOC whose track has its pregap in the file, and
  such a TOC is still refused. cdrdao 1.2.5 names its version only in its
  usage, where 1.2.4 named it before every command's output; the worker now
  takes the version from its probe at start, where it took a command
  without the banner for a binary that was not cdrdao, which with 1.2.5
  would have refused every CD burn. The opt-in decryption image's build
  stages are on trixie too (dvdbackup 0.4.2-5); rebuild it from the new
  release. CI runs the engine tests in the same Debian, so they test the
  versions that ship.
- Burning an artifact pending deletion is refused with `ARTIFACT_DELETED`
  (409), not `VALIDATION_FAILED`. No version set the old flag; a row that
  had it, set by hand, is reset to kept by migration 0033.
- A CHD imported as it is goes to RomM as it is once `chdman verify` has
  passed it. The export runs the verify once per CHD and chdman version, on
  a server whose derivation engine is chdman, and records the verdict by the
  CHD's SHA-256 (migration 0032), only when chdman itself answered: a run
  that could not start, took too long or was ended by a signal is tried
  again on the next pass. One that fails is not sent, and the edition's
  status says what chdman said, the library's path left out. A CHD made and
  checked here is still preferred, then a verified imported one, then an ISO
  or CUE/BIN.
- **A worker refuses plain HTTP that would leave its machine** (breaking
  for a remote worker given an `http://` server URL). An `http://`
  `TANGIBLE_SERVER_URL` is accepted only where the server's address is
  loopback or, in a container, on a network the container is attached to,
  as the Compose network in `compose.hardware.yaml` is; elsewhere the worker
  stops at start and says which address and why. Use an `https://` URL, or
  set the new `TANGIBLE_WORKER_PLAIN_HTTP=true` to accept plain HTTP across
  the network. A server URL carrying a user name or password is refused,
  and the worker no longer follows redirects. The worker protocol has
  always required this; the worker did not enforce it.

### Fixed

- Burns and discs without a label were named in the web UI by the first
  eight characters of their identifier, which in a UUIDv7 are the time it
  was made: two burns queued within a minute or so had the same name. They
  are named by the identifier's last eight characters now, which are
  random, and a burn's page lists its whole identifier.
- The web UI's smallest font files were built into its stylesheet as
  `data:` URIs, which its Content Security Policy refuses, so the
  characters they cover fell back to a system font. Assets are never
  inlined now; each is served as a file from Tangible's own origin.

- A drive that drops off its bus while xorriso reads what is in it (a USB
  drive whose hub resets, seen on the bench) is reported as unavailable.
  xorriso had already printed a summary naming a blank CD-R and nothing of
  its capacity, and the worker read that as a CD-R it could not write.
- A burn worker asked to stop while it writes or erases a disc now
  finishes first, the read-back and report included, and then stops.
  It used to stop at once: the writing tool, which runs inside the worker's
  container, was killed with it, so the disc was ruined, and the worker
  came back with an attempt only a person could account for. Anything else
  (a download, a preflight, a wait for a disc, a rip) still stops at once.
  Compose now gives a worker two hours to stop (`stop_grace_period`) and
  the server a minute, where Docker's default is ten seconds, which was
  shorter than the server's own 30 seconds for requests under way.
- A CHD sent to RomM is recorded in its export receipt as `chd`; it was
  recorded as `cue_bin`.
- A burn worker no longer writes its server URL to the log before checking
  it. A password in `TANGIBLE_SERVER_URL` reached the start-up log line;
  such a URL is now refused before anything is logged.
- A CHD is refused for burning, on the server and on its page. With no
  track layout a worker would have written it as a data disc, putting the
  CHD's compressed bytes on the medium rather than the disc they hold.
- A database connection can no longer be lent out still inside a
  transaction. sqlx gave one back that way when a transaction's `BEGIN`
  was cut short (a client that went away mid-request, a timeout), and every
  statement run on it afterwards was silently never committed and held its
  locks: an import could wait forever on a lock held by its own earlier
  progress update. Each connection given back is now checked and, if inside
  a transaction, closed, which rolls it back; and PostgreSQL ends any
  transaction left idle for five minutes.
- The account list is paged (`limit` and `cursor` on `GET /api/v1/users`,
  `next_cursor` in the answer), like every other list, rather than sent
  whole however many accounts there are. The Accounts page shows the first
  50 and a "Show more accounts" button.
- The block reader reads a DVD with direct I/O, around the page cache.
  Through the cache, the kernel read ahead of the sector asked for, so a
  refused sector a few dozen ahead made good ones fail and the error was
  blamed on the wrong sector; a damaged disc failed before it needed to. A
  whole disc no longer passes through the cache either. Where direct reads
  are refused, the reader falls back to cached ones.
- A completed import now removes its staging copy. Every import since 0.1.0
  kept a second full copy of its bytes in the staging directory, doubling
  the disk it used; those left by earlier versions can be deleted from
  `staging/imports/` for any import shown as complete.

### Known limitations

- Blu-ray discs cannot be ripped, and BDMV images are not recognised; the
  opt-in decryption image decrypts CSS DVDs only.
- A `VIDEO_TS` folder is kept and ripped, but cannot be burned.
- chdman 0.276 still misreads a cdrdao TOC whose track has its pregap in the
  file, so such a TOC is refused rather than converted.
- Audio tracks are verified by length and readability, not compared, because
  without the drive's read offset a comparison fails on correct hardware.
- The library is stored on a local filesystem; S3-compatible storage is an
  import source, not yet a store.
- No metadata providers yet.
- Tested on one drive so far, with CD-R and DVD-RW; see "Tested hardware" in
  `deploy/README.md`.

## 0.1.0 (2026-10-07)

The first release.

### Library

- Import by upload, or from watched folders an administrator configures. A
  folder holding a disc's files imports as one artifact; symbolic links are
  refused, and hidden files are left out.
- ISO 9660 and UDF images, CUE/BIN and cdrdao TOC/BIN are recognised from
  their structure, with warnings for anything structurally odd. CUE and TOC
  references are resolved inside the import only.
- Content-addressed storage by SHA-256: every file stored once, read-only,
  never renamed or rewritten. Each artifact has a portable manifest
  (`org.tangible.artifact-manifest/v1alpha1`).
- A catalogue of titles, editions, disc sets and discs, with images linked to
  discs.

### Burning

- One burn worker per optical drive, in its own container with only that
  drive's block device and group. Workers enrol with one-use tokens, hold
  leases on their work, and recover safely after a restart without ever
  writing twice.
- xorriso writes ISO images to CD, DVD and Blu-ray media; cdrdao writes CD
  layouts with audio, mixed mode, pregaps, catalogue numbers and ISRCs.
- Preflight checks the disc before any write; the input is staged and
  re-hashed on the worker first.
- Read-back verification: ISO images compared in full; CD layouts track by
  track, data compared byte for byte and audio measured, recorded as passed
  only when everything was compared and as partial otherwise.
- An inventory of physical copies and how each was verified.
- Erasing a rewritable disc is its own operation, for administrators only,
  with an explicit confirmation. A burn never erases.

### Derivatives

- CHDs of CUE/BIN, TOC/BIN and CD ISO images, made by chdman and kept only if
  they extract back identical to the original, track by track. Lineage is
  recorded and shown, and asking twice makes one.

### RomM

- Chosen games are written into RomM's library folder as RomM scans it, one
  folder per game with its discs together, preferring a checked CHD. Only
  folders Tangible created are ever changed.

### Operations

- A web UI served by the server, and a REST API described by a generated
  OpenAPI document.
- Accounts with viewer, operator and administrator roles, cookie sessions,
  CSRF protection, sign-in rate limits, and an audit trail.
- Hardened containers checked in CI, parsers fuzzed, secrets kept out of logs
  and help text, and release images signed with SBOM and provenance.
- The licence notices of every bundled Rust crate and JavaScript package,
  generated at build time, in the image and with each release.
- Release images are scanned for vulnerabilities before they are signed, and
  one with a fixable high or critical vulnerability is not released.
- `SOURCES.md` with each release links every Debian package in the image to
  its archived source and its upstream project, and every page of the web UI
  links to Tangible's source.

### Known limitations

- Imports come only from uploads and watched folders; URL and download-client
  imports are not available yet.
- CCD/IMG/SUB, MDS/MDF, BDMV, VIDEO_TS and imported CHD images are not
  recognised.
- CHDs of DVDs need chdman 0.262 or later; the image carries 0.251.
- chdman 0.251 misreads a cdrdao TOC whose track has its pregap in the file,
  so such a TOC is refused rather than converted.
- Audio tracks are verified by length and readability, not compared, because
  without the drive's read offset a comparison fails on correct hardware.
- The RomM export writes files; it does not yet ask RomM to rescan.
- Storage is a local filesystem; S3-compatible storage is not available yet.
- Tested on one drive so far; see "Tested hardware" in `deploy/README.md`.
