# Changelog

Notable changes to Tangible, newest first. Versions follow semantic
versioning; during 0.x the API, the manifest schema and the database schema
may change between minor versions, and the release notes say how.

## Unreleased

### Added

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
- Worker enrollment is rate limited, as AGENTS.md requires of worker
  registration: after five rejected tokens under one worker name in
  fifteen minutes, or sixty in all, further rejected tokens are answered
  429 `RATE_LIMITED` with `Retry-After` until the window passes, without a
  log line each. A valid token always enrolls, so bad tokens cannot lock a
  real worker out.
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
  to hide it. Diagnostics gains a "Removable roots" check (ADR 0017).
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
- CDs can be ripped into the library (ADR 0013, first part): an operator
  asks for a rip of a drive's disc (`POST /api/v1/drives/{id}/rips`), the
  drive's worker reads it with `cdrdao read-cd`, sends the table of
  contents and data, and they are imported as a TOC/BIN artifact whose
  origin says it was dumped from a disc. A rip can be withdrawn until its
  import starts. DVDs, Blu-rays and blank discs are refused for now. A
  drive now does one thing at a time across burns, erasures and rips. The
  web UI's Rips page asks for a rip and follows it to the library, with
  progress as the disc reads.
- DVD data discs can be ripped too (ADR 0013, second part): the worker
  reads the disc sector by sector from the drive itself into an image,
  retrying a sector that fails and stopping at one that will not read, and
  it is imported as an ISO. An encrypted (CSS) DVD is recognised while it
  reads and refused, keeping nothing, including on drives that will not
  read its video at all without authentication. A disc the system has not
  caught up with since a burn is refused with a request to reload it.
  Blu-rays are still refused.
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
- The opt-in worker image also installs redumper (ADR 0015), from its
  GitHub release pinned by build and SHA-256 for amd64 and arm64, for CD
  rips with the drive's read offset corrected. The worker does not use it
  yet. It is not in the published image, and the release's check refuses
  it there, because it carries its own CSS and AACS code.
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

### Fixed

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
