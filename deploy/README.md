# Deployment

Three Compose files. Combine them; do not edit them in place.

| File | Contains | Device access |
|---|---|---|
| `compose.yaml` | server, PostgreSQL | none |
| `compose.dev.yaml` | development overrides, local build, exposed database | none |
| `compose.hardware.yaml` | one burn worker per optical drive | `/dev/srN` |
| `compose.hardware-sg.yaml` | the drive's generic SCSI node, on top of the above, only for a drive that needs it | `/dev/sgN` |

## Development

```bash
cp deploy/.env.example deploy/.env
docker compose -f deploy/compose.yaml -f deploy/compose.dev.yaml up --build
```

The fake burn engine is the default. Nothing in this combination can reach an
optical drive.

The image serves the web UI at the server's own address, beside the API. For
live reloading while working on the UI, run `pnpm --dir web dev` instead and
open the address it prints; it forwards API requests to the server on port
8080.

## Production

```bash
cp deploy/.env.example deploy/.env
docker compose -f deploy/compose.yaml config                 # read the rendered output
docker compose -f deploy/compose.yaml up -d postgres
docker compose -f deploy/compose.yaml run --rm server migrate
docker compose -f deploy/compose.yaml up -d
```

Review the rendered configuration before starting: image references, volume
mappings, ports, and the env file. Pin an image digest rather than a tag for a
real deployment.

Migrations are a step of their own, never run behind your back. Until they are
applied the server says so in its log at every start, and its healthcheck
fails, so `docker compose ps` shows it unhealthy and no burn worker starts
against it.

The web UI and the API share one address, `TANGIBLE_PUBLIC_URL`: the server
serves the UI's built files itself, so there is no separate web container and
no Node runtime in the image. `TANGIBLE_WEB_ROOT` points at the build, and the
image sets it; unset, the server answers the API alone.

## First sign-in

Every route except the health probes and the way in needs a signed-in account.
A fresh server has none, and logs a warning until one exists: whoever completes
setup first becomes the administrator, so do it as soon as the server is up.

Open `TANGIBLE_PUBLIC_URL` in a browser: on a fresh server the first visit
lands on the setup page. From a shell instead, with
the password typed into the terminal rather than put on the command line, where
it would stay in shell history:

```bash
umask 077
jar=$(mktemp)
# Type {"username":"owner","password":"at least twelve characters"}, then
# Ctrl-D. Use /api/v1/session instead of /api/v1/setup to sign in later.
curl -sS -c "$jar" -H 'Content-Type: application/json' -d @- \
  http://localhost:8080/api/v1/setup
```

The response carries a `csrf_token`. Every request that changes something
sends it back, with the cookie:

```bash
curl -sS -b "$jar" -H 'Content-Type: application/json' \
  -H 'X-CSRF-Token: <csrf_token from the response>' -d '{}' \
  http://localhost:8080/api/v1/worker-enrollments
rm -f "$jar"
```

The cookie jar holds a live session; remove it when done. Further accounts are
created by an administrator, from the web UI's Accounts page or
`POST /api/v1/users`, as `viewer`, `operator` or `administrator`.

`TANGIBLE_PUBLIC_URL` decides how the session cookie is marked. An `https://`
URL gets a `Secure` cookie. An `http://` URL gets one a browser will send back
over plain HTTP, so a server on a LAN without TLS still works, and the server
logs a warning at every start, because passwords and session cookies then cross
the network unencrypted. Put HTTPS in front of Tangible before exposing it to
any network you do not trust.

## Adding a real drive

Hardware is opt-in and deliberately separate, so no default or development
stack can reach a drive by accident.

```bash
ls -l /dev/sr*                   # identify your drive
stat -c %g /dev/sr0              # the group that owns it
# set TANGIBLE_BLOCK_DEVICE, TANGIBLE_OPTICAL_GID and TANGIBLE_WORKER_NAME in .env

docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml config
# read the rendered output, confirm the device mappings are the drive you meant
docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml up -d
```

One worker per drive. To add a second, copy the `burner-sr0` service, rename
it, and map that drive's own block device.

## Tested hardware

What has actually been burned and checked, through the worker as it deploys
(unprivileged, in its container, with only the block device):

| Drive | Connection | Media | Result |
|---|---|---|---|
| Slimtype DVD A DS8A8SH, firmware KS21 | USB | CD-R, 80 minutes (CMC Magnetics) | ISO through xorriso, read back identical; mixed-mode CUE/BIN through cdrdao, data track identical, audio measured |
| Slimtype DVD A DS8A8SH, firmware KS21 | USB | DVD-RW (Ritek) | burned and read back identical, a second burn refused, erased on request, burned again and read back identical |

Nothing else has been tried yet, including DVD-R, DVD+R and Blu-ray media,
which the engines support but which have not been through a real drive here.
Reports of other drives and media, with the burn form on the issue tracker,
are how this table grows.

## Why one device node

Both engines, xorriso and cdrdao, send their drive commands through the block
device itself. On the first real drive they probed it, inspected the medium,
burned a disc and read it back with nothing else mapped, on a host that had no
generic SCSI nodes at all. So the worker gets the block device and not the
generic SCSI node beside it.

If a drive turns out to need the generic node, add it on top:

```bash
lsscsi -g                        # find the /dev/sgN that pairs with the drive
# set TANGIBLE_SCSI_DEVICE in .env
docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml \
  -f deploy/compose.hardware-sg.yaml config
```

The pairing has to be looked up: `/dev/sr0` does not reliably go with
`/dev/sg0`.

## Why the worker needs a group

A mapped device keeps its host owner and mode inside the container, and the
image runs as an unprivileged user rather than root. The drive node normally
belongs to root and a group such as `optical` or `cdrom`, readable and writable
by that group only, so the worker is added to that group and to nothing else.
The group's number differs between distributions, which is why
`TANGIBLE_OPTICAL_GID` has no default and Compose refuses to start the worker
until it is set.

## Showing games in RomM

Tangible can write chosen games into RomM's library folder, laid out the way
RomM scans it: `{platform}/{Title} ({Region})/`, with a multi-disc game's
discs together in one folder. It is off until you give the server RomM's
`roms` directory:

1. In `compose.yaml`, uncomment `TANGIBLE_ROMM_EXPORT_ROOT` and the matching
   volume, and set the host side of the volume to the `roms` folder of RomM's
   library.
2. Make that folder writable by the server's user, uid 10001 in the image.
   RomM only needs to read it.
3. Run `docker compose -f deploy/compose.yaml config`, check the mount, and
   restart the stack.

Then, in the catalog, open an edition, choose its platform and tick "Show this
game in RomM". The status underneath says when the folder has been written,
or why it could not be. Unticking removes the folder again. Scan the library
in RomM to pick up changes.

Tangible writes only folders it created, each marked with a
`.tangible-export.json` file, and never modifies or removes anything else in
RomM's folder. If a folder of the same name already exists and is not
Tangible's, the game is reported as blocked rather than written over.

Files are hard-linked from the library when the export folder is on the same
filesystem, and copied otherwise. With the library in a named volume and
RomM's folder bind-mounted, as above, they are copied, so allow the space.

For each disc the export takes a checked CHD when the disc has one (see
"Making CHDs"), otherwise its ISO or CUE/BIN. A disc imported only as a cdrdao
TOC reaches RomM through its CHD, since RomM's emulators do not read TOC
files. A CHD that was not checked against its original is never exported.

## Making CHDs

The image carries chdman, from MAME, so the server can make a CHD of a disc
image: a compressed copy that RomM and its emulators read. Nothing is converted
on its own. Ask for one artifact at a time, with "Make a CHD" on its page in
the library, or from a shell:

```bash
curl -sS -b "$jar" -H 'Content-Type: application/json' \
  -H 'X-CSRF-Token: <csrf_token>' -d '{}' \
  http://localhost:8080/api/v1/artifacts/<artifact id>/derivatives
```

The CHD becomes a new artifact, linked to the same disc as its original, and
the original is never changed. Before it is kept, the CHD is extracted again
and compared with the original track by track; one that does not come back
the same is refused, and the job says which track differed.
`GET /api/v1/artifacts/<id>/lineage` shows what was made from what.

CUE/BIN images and CD ISOs work. The chdman in this image (0.251) cannot make
DVD CHDs, and it misreads a cdrdao TOC whose track has its pregap in the file,
so such a TOC is refused rather than converted wrongly.

A conversion needs room in the staging volume for a copy of the original and
the CHD, briefly. Set `TANGIBLE_DERIVATION_ENGINE=none` to turn conversions
off.

## Importing from a URL

Off unless you turn it on. With `TANGIBLE_URL_IMPORTS=true` in `.env` and a
restart of the server, operators get "Import from a URL" on the Imports page
(and `POST /api/v1/imports/url`): the server fetches the file into staging,
resumes it if the connection drops, and imports it like an upload. An
optional SHA-256 is checked before anything is imported.

The server fetches only over https, only from public addresses, and only on
ports 80 and 443. It refuses this machine, your LAN, the cloud metadata
address and every other private or reserved range, checks every redirect the
same way, and records the address without its query string, which is where
signed links keep their secret. Each request is audited, a refused one
included, and an account may ask for 30 an hour. To loosen it:

- `TANGIBLE_URL_IMPORTS_ALLOW_HTTP=true` allows plain http as well.
- `TANGIBLE_OUTBOUND_ALLOW=192.168.1.0/24,fd00::/8` lets fetches reach those
  blocks although they are private. Only list what you mean: anyone who may
  import can then make the server fetch from there.
- `TANGIBLE_OUTBOUND_PROXY=http://proxy:3128` sends every fetch through a
  proxy. The environment's `HTTP_PROXY` is never used. A proxy resolves names
  itself, so it must block private addresses itself too. Treated as a secret,
  since its URL may carry a username and password: never logged or shown.

A file is fetched up to `TANGIBLE_MAX_UPLOAD_BYTES`. Diagnostics shows whether
URL imports are on and how far they reach.

## Sending torrents to qBittorrent

Tangible can hand a torrent you already have (a `.torrent` file or a magnet
link) to a qBittorrent you run, and import it when it finishes. It adds the
torrent to one category and nothing else, never searches for anything, never
asks qBittorrent to run a script, and never removes a torrent or its data:
seeding and cleaning up stay qBittorrent's business.

1. Give Tangible's server read-only access to the category's download
   folder. If qBittorrent saves `tangible` torrents to `/downloads/tangible`
   in its own container, and that is `/srv/torrents/tangible` on the host,
   add to the server in `compose.yaml`:

   ```yaml
       volumes:
         - /srv/torrents/tangible:/var/lib/tangible/torrents:ro
   ```

   and name it as a watched root in `.env`:
   `TANGIBLE_WATCH_ROOTS=torrents=/var/lib/tangible/torrents`.
2. Make an integration secret key (below), so the WebUI password can be
   stored.
3. As an administrator, add the qBittorrent on the Integrations page: its
   WebUI address, a user and password, the category, the download folder as
   qBittorrent sees it (`/downloads/tangible`), and the watched root
   (`torrents`). "Test" signs in and shows the WebUI's API version.
4. Operators hand it a torrent under "Send a torrent to a download client" on
   the Imports page: a `.torrent` file or a magnet link. (The API is
   `POST /api/v1/integrations/{id}/jobs`.)

A LAN address is fine. The address must use the port qBittorrent's WebUI
itself listens on: with its host header validation on (the default), a
request to a remapped port is turned away before the password is looked at.
A qBittorrent on this machine's loopback address is reached only if you list
it in `TANGIBLE_OUTBOUND_ALLOW` (such as `127.0.0.1/32`).

While a torrent downloads, its import shows "Downloading in qBittorrent" with
its progress. If qBittorrent cannot be reached, or the password is wrong, the
import shows "Waiting for qBittorrent" and why, and the integration shows as
failing. A torrent
qBittorrent reports in error, removes, or keeps outside the category's
folder ends the import.

## Sending NZBs to SABnzbd

Tangible can hand an NZB you already have to a SABnzbd you run, and import
the download when it completes. It never searches for anything, asks for no
script, and never removes a job or its files.

1. In SABnzbd, add a category (Config, Categories), `tangible` by default,
   with its own folder. SABnzbd files a category it does not know under its
   default one, so Tangible refuses to add anything until it exists.
2. Give Tangible's server read-only access to that category's finished
   folder, as for qBittorrent above: mount it, and name it in
   `TANGIBLE_WATCH_ROOTS`.
3. Make an integration secret key (below), so the API key can be stored.
4. As an administrator, add it on the Integrations page, as kind SABnzbd:
   its address, the category, the finished folder as SABnzbd sees it, the
   watched root, and its API key (Config, General). "Test" checks the key
   and the category.
5. Operators send it an `.nzb` under "Send to a download client" on the
   Imports page. The same NZB again is answered with the import already
   made, and not downloaded twice. (The API is
   `POST /api/v1/integrations/{id}/jobs`, `{"nzb": "<base64>"}`.)

Use SABnzbd's IP address, or add its host name to `host_whitelist` in its
special settings: SABnzbd turns away host names it does not know.

## Sending NZBs to NZBGet

The same as SABnzbd above, with NZBGet's own words:

1. In NZBGet, add a category (Settings, Categories), `tangible` by default.
   NZBGet takes a category name it does not know and files the download
   under it, outside the folder a category of its own would have, so
   Tangible refuses to add anything until it exists.
2. Mount its finished folder (`DestDir`, plus the category's folder) read-only
   into Tangible's server and name it in `TANGIBLE_WATCH_ROOTS`.
3. Add it on the Integrations page, as kind NZBGet: its address, the
   control user and password, the category, the finished folder as NZBGet
   sees it, and the watched root. A restricted user cannot read the
   categories, and an add-only user cannot follow a job. Operators send it
   an `.nzb` from the Imports page, as for SABnzbd.

Tangible appends with no post-processing parameters and NZBGet's duplicate
check off, since it keeps its own; a script you set on the category is
yours, and runs as you set it.

## Importing from S3-compatible storage

Operators can import an object from a bucket you allow on an S3-compatible
service (AWS S3, or a self-hosted one such as SeaweedFS or Garage). Tangible
only reads: it asks what an object is and fetches it, never lists, writes or
deletes.

1. Make credentials that may read the buckets you mean to allow, and nothing
   else.
2. Make an integration secret key (below), so they can be stored.
3. As an administrator, add it on the Integrations page, as kind
   S3-compatible storage: the endpoint, the region (us-east-1 if unsure),
   whether buckets go in the path (on for most self-hosted servers; off for
   AWS's newer buckets), the buckets operators may import from, and the
   access key id and secret. "Test" checks every bucket.
4. Operators use "Import from S3" on the Imports page: the bucket and the
   object's key. (The API is `POST /api/v1/imports/s3`.)

An endpoint on your LAN is fine; one on this machine's loopback address only
if listed in `TANGIBLE_OUTBOUND_ALLOW`. The import is pinned to the version
and size the object had when it was asked for. In a bucket with versioning,
writing the key again meanwhile does not change what is imported. In one
without, a rewrite of a different size fails the import, and one of the same
size is not noticed: give the SHA-256 when you need to be sure of the
content.

## Integration secrets

Download clients (qBittorrent, and SABnzbd and NZBGet later) sign in with a
password. You enter it once, as an administrator, and Tangible stores it
sealed with a key kept outside the database; nobody can read it back, through
the web UI or the API. Without a key the server runs, and an integration
cannot be given a password.

Make the key once, readable only by the server's user (10001 in the image):

```bash
mkdir -p deploy/secrets
docker compose -f deploy/compose.yaml run --rm --no-deps server \
  secrets generate > deploy/secrets/tangible-secret-key
chmod 0400 deploy/secrets/tangible-secret-key
sudo chown 10001 deploy/secrets/tangible-secret-key
```

Then uncomment `TANGIBLE_SECRET_KEY_FILE` and the two `secrets` blocks in
`compose.yaml`, and restart the server. Diagnostics says whether a key is
loaded and whether it opens every stored password.

Back the key up apart from the database: a database backup holds the
passwords sealed, and the key is what opens them. A lost key loses only those
passwords; enter them again. To move to a new key, make one as above under
another name, then:

```bash
docker compose -f deploy/compose.yaml run --rm \
  -v "$PWD/deploy/secrets:/keys:ro" server \
  secrets rotate --old-key-file /keys/old-key --key-file /keys/new-key
```

and point `compose.yaml` at the new file. A rotation changes everything or
nothing, and can be run again.

## Issue reports

Tangible can turn a problem into a prefilled issue on its tracker. The
server only prepares the text: credentials, tokens, paths on the host, drive
serial numbers, the library's file names and, by default, LAN addresses and
local host names are replaced with placeholders such as `<path>.bin`. The
operator reads all of it, and only then does their own browser open the
issue form. Nothing is sent by the server, and nothing at all without that
click.

On a burn's page, an operator can "Report a problem with this burn", or,
for a disc that burned and checked on a real drive, "Report this drive and
disc as working". The second grows the tested hardware table above, so it
is worth doing for any drive or disc type not yet in it. An error that means
Tangible itself went wrong offers "Report this…" beside it, and the
diagnostics report below can be sent the same way. An image Tangible did
not recognise, or read with doubts, offers "Ask for support…" on its page:
that describes the image's structure (sizes, extensions, the first bytes of
binary files, known signatures), never its contents or names.

Two settings in `.env`:

- `TANGIBLE_REPORT_ISSUES_URL`: the issue form reports open. Set it empty to
  stop offering reports, for example on a network without internet access or
  where staff should not open public issues. If you run a modified Tangible,
  point it at your own tracker.
- `TANGIBLE_REPORT_SCRUB_ADDRESSES`: `false` keeps IP addresses and host
  names in reports; everything else is always replaced.

## Verifying a release

Each release is built by `.github/workflows/release.yml` from its tag, and
signed by that workflow with a short-lived Sigstore certificate: there is no
long-lived key to steal, and a signature proves which workflow, at which tag,
in which repository, built the bytes. The release page gives the image's
digest. Use the digest, not the tag, in `compose.yaml`; a tag can be moved,
a digest cannot.

With [cosign](https://docs.sigstore.dev/cosign/system_config/installation/)
3.0 or later, from your distribution or its release page. Releases are signed
with cosign 3, which stores the signature in the newer Sigstore bundle format;
cosign 2 does not look for it there and reports "no signatures found" for a
correctly signed image. `cosign version` says which you have.

```bash
IMAGE=ghcr.io/digital-grease/tangible@sha256:<digest from the release page>
TAG=v0.1.0

# The image was signed by this repository's release workflow at this tag.
cosign verify "$IMAGE" \
  --certificate-identity "https://github.com/digital-grease/tangible/.github/workflows/release.yml@refs/tags/$TAG" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The release files are covered by `SHA256SUMS`, which is signed the same way:

```bash
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity "https://github.com/digital-grease/tangible/.github/workflows/release.yml@refs/tags/$TAG" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
sha256sum --check SHA256SUMS
```

GitHub's build provenance covers the image and every release file as well,
for anyone using the GitHub CLI:

```bash
gh attestation verify "oci://$IMAGE" --repo digital-grease/tangible
gh attestation verify tangible-deploy-$TAG.tar.gz --repo digital-grease/tangible
```

Each release also carries `sbom.spdx.json`, the image's software bill of
materials, and `SOURCES.md`, which names the Debian source package and exact
version of everything in the image, with a link to that version's archived
source and to the upstream project: publishing the image conveys object code
for GPL programs such as xorriso and cdrdao, and that is where their source
is.

The web UI links to Tangible's source from every page, as the AGPL expects of
a network service. If you run a modified Tangible for other people, point
`SOURCE_URL` in `web/src/lib/source.ts` at your modified source.

`THIRD_PARTY_NOTICES-rust.txt` and `THIRD_PARTY_NOTICES-web.txt` carry the
licence notices of the Rust crates compiled into Tangible and of the
JavaScript bundled into its web UI. They are generated while the image is
built, so they match what shipped, and the image carries them too, under
`/usr/share/doc/tangible/`.

Before an image is signed, the release workflow scans it with Grype, and a
high or critical vulnerability with a fix available stops the release. The
image applies Debian's security updates when it is built, so a rebuild picks
up fixes published after the base image.

## Ripping a disc

Put a CD or DVD you own in a drive, open Rips in the web UI, and choose
"Rip the disc in" that drive. The drive's worker reads it when the drive is
free: a CD with `cdrdao read-cd` into a table of contents and its data,
imported as TOC/BIN; a DVD sector by sector from the drive itself into an
image, imported as an ISO. Either is kept exactly as read, padding and all.
Reading changes nothing on the disc, so a rip can be withdrawn while it
reads. Blank discs and Blu-rays are refused. An encrypted DVD is
recognised while it reads and refused with nothing kept, unless the rip
asked to decrypt it on a worker that can (below).

Straight after Tangible burns a disc, the system may still think the drive
holds the old, blank disc. A rip then refuses ("take the disc out and put
it back") rather than reading a fragment; reloading the disc fixes it.

Ripped audio CDs are not read-offset corrected, so their audio will not
match AccurateRip or Redump databases. Data tracks are unaffected. The
image below also carries redumper, which will correct the offset on the
workers that run it (ADR 0015); the worker does not use it yet.

### Ripping encrypted discs

The image Tangible publishes contains no decryption code, and the release
checks that before it signs one. Like VLC, which leaves libdvdcss and the
AACS libraries to whoever wants them, Tangible leaves them to you: a worker
that rips encrypted discs runs an image you build yourself, on top of the
release you run. Whether ripping a disc is lawful where you are is yours to
determine.

Read [`decryption/Dockerfile`](decryption/Dockerfile) first; it is short. It
fetches libdvdcss from VideoLAN's source release, checks it against the
SHA-256 written there, and builds it; it installs Debian's `dvdbackup`,
`libaacs0` and `libbdplus0`; and it fetches redumper's Linux build from its
GitHub release, checked against the SHA-256 written there for your
architecture (amd64 or arm64). redumper is for CD rips with the read offset
corrected, and is here rather than in the published image because it also
carries its own CSS and AACS code; the worker only ever runs its
`drive::detect`, `dump` and `split`, on a CD. Then build it from the release you run, and
check it carries what you expect:

```bash
docker build -f deploy/decryption/Dockerfile \
  --build-arg TANGIBLE_IMAGE=ghcr.io/digital-grease/tangible:v0.1.0 \
  -t tangible-decryption:v0.1.0 deploy/decryption
docker run --rm --entrypoint cat tangible-decryption:v0.1.0 /usr/share/tangible/decryption.json
```

Run the drive's worker from it with one more override, which changes only
the image:

```bash
TANGIBLE_DECRYPTION_IMAGE=tangible-decryption:v0.1.0 docker compose \
  -f deploy/compose.yaml -f deploy/compose.hardware.yaml \
  -f deploy/compose.decryption.yaml up -d
```

The worker logs "this worker's image can decrypt DVDs" at start, with the
library and tool versions, and reports them to the server; a warning
instead means the image is not as the Dockerfile builds it. The worker keeps
the same confinement: its drive's block device and group, no capabilities, a
read-only root. On the first drive tried, libdvdcss
authenticated and found every title key with nothing more.

The server refuses every request to decrypt until an administrator turns it
on, with `TANGIBLE_ALLOW_DECRYPTION=true` in `.env` and a restart of the
server. A rip that asks to decrypt is then accepted only for a drive whose
worker reported it can; Diagnostics shows whether the setting is on. Turning
it off again refuses decrypting rips still waiting for their drive.

That image is yours, not a Tangible release: it is outside the release's
signature, SBOM, provenance and vulnerability scan. Rebuild it each time you
upgrade, so it carries the new release and Debian's latest fixes.

## Diagnostics

When something is wrong and the cause is not obvious, ask for the whole
picture:

```bash
docker compose -f deploy/compose.yaml run --rm server doctor --report
```

It works when the web UI does not, and prints a checklist: the database and
its schema, whether the library and staging directories are there, writable
and have room, how many artifacts integrity checks found damaged and when
the last full scrub ended, the derivation tool, the RomM export, every
worker (version, protocol, when it was last heard from, its engines), every
drive (vendor, model, firmware, connection, status), the codes of the last
20 failures, and the settings that matter for support. Each line is OK, Warning or Problem,
and the command exits non-zero when any is a Problem. `--report --json`
gives the same as JSON for scripts.

The report is written to be shared. It names no paths, no file names from
the library, and no worker or drive names: they are numbered. Failures are
codes and times, not their details. When issue reports are on, it ends with
a link to a bug report filled in with everything but the report itself,
which you paste into its Logs field.

Administrators see the same checklist in the web UI under Diagnostics, with
buttons to copy it, download it, or prepare it as an issue.

Plain `doctor`, without `--report`, is the container healthcheck: it only
checks that the database answers and its schema is current.

## Finding an error in the logs

Every error the web UI or the API shows carries a request id, such as
`01a126e9-2507-7133-8bfa-b724fc014c1f`, and every log line the server wrote
while handling that request carries the same id. To see what happened:

```bash
docker compose -f deploy/compose.yaml logs server | grep 01a126e9-2507-7133-8bfa-b724fc014c1f
```

Request start and finish lines are logged at debug level; set
`TANGIBLE_LOG=tangible=info,tower_http=debug` to see them as well as errors.

## Backups

Three volumes hold everything that cannot be rebuilt:

| Volume | Holds |
|---|---|
| `postgres-data` | the catalogue, jobs, burn history, physical copies, accounts, audit trail |
| `library` | every stored file and every artifact's manifest |
| each worker's `burner-*-cache` | its credential, and its recovery record while a burn is in progress |

`staging` holds only work in progress and needs no backup.

Back up the database with PostgreSQL's own tool, and the library with whatever
you use for large files; it only ever grows, and nothing in it is rewritten:

```bash
docker compose -f deploy/compose.yaml exec -T postgres \
  pg_dump -U tangible -Fc tangible > tangible-$(date +%F).dump
```

Take the database dump before the library copy, so every artifact the
database names is in the copy. A backup is not a backup until a restore from
it has been tried.

To restore: stop the stack, restore the `library` volume, start only
PostgreSQL, restore the dump with `pg_restore --clean --if-exists`, then start
the rest.

## Checking stored bytes

A check reads stored files back and compares each with the SHA-256 digest
and length recorded when it was stored, to find a failing disk, bit rot or a
file changed by hand before you need it. A check of one artifact also
confirms its manifest still names the files the catalogue does. Nothing is
ever repaired: originals are never rewritten, so the remedy is your backup.

From the command line, the whole library or one artifact:

```bash
docker compose -f deploy/compose.yaml run --rm server scrub
docker compose -f deploy/compose.yaml run --rm server scrub --artifact <artifact id>
```

It says how far it has got, lists what it found, and exits non-zero when it
found anything, so a job you schedule yourself notices. It works whether or
not the server is running; if the server is already reading the same check,
it follows that one. Stopped with Ctrl-C, it gives the check up, and the
server, or the next `scrub`, goes on from where it got to. `--no-wait`
queues the check and leaves it to the server.

The server can queue a full scrub itself: set `TANGIBLE_SCRUB_INTERVAL_DAYS`
to the number of days after one ends before the next is queued. It is off
by default, because a full scrub reads every byte in the library and that
load is yours to decide on. Turned on, the first is queued when the server
starts. The web UI offers the same: "Verify now" on an artifact's page,
for operators, and the Storage page, for administrators, which runs and
withdraws full scrubs and lists every check with what it found. So does the
API (`POST /api/v1/integrity-checks`, for administrators, and
`POST /api/v1/artifacts/{id}/validate`, for operators).

What a check can find:

- `missing`: the file is not in the library.
- `corrupt`: it reads back as other bytes than were stored.
- `size_mismatch`: it is longer or shorter than was stored.
- `unreadable`: reading it failed; the server log says why.
- `manifest_mismatch`, `manifest_unreadable`: the artifact's manifest no
  longer agrees with the catalogue, or cannot be read.

Every artifact naming a damaged file is marked damaged, and `doctor --report`
shows how many are as a Problem, with when the last full scrub ended and
what it found. Each problem names the file's digest. The file lives in the
`library` volume at `objects/sha256/<first two>/<next two>/<digest>`, and a
manifest at `manifests/<artifact id>.json`. Copy that path back from your
backup, keep it read-only as Tangible left it, then check the artifact
again: one that reads back intact is marked intact.

### Finding files nothing needs

Tangible never removes a stored file yet, but it can say which it would:

```bash
docker compose -f deploy/compose.yaml run --rm server gc
```

It lists stored files that nothing refers to (no catalogued artifact,
published manifest, import still under way, reference count or retention
lock) and that are older than a grace period, 7 days unless you pass
`--grace-days`, with what removing them would free. It also counts partial
files left by uploads or imports that were interrupted, entries in the
object tree that are not stored files, and files the catalogue records that
are missing (`scrub` says which artifacts those belong to). `--limit` sets
how many files it names, largest first; `--json` prints the same plan the
API gives administrators (`GET /api/v1/storage/gc-plan`), which the Storage
page shows when you ask for it. If a manifest or
an import's checkpoint cannot be read, the plan says it is uncertain, since
the unread one might name a listed file.

The plan only reads; it removes nothing. If you remove a listed file by
hand, stop the server first: an import that has just stored the same bytes
again may not have recorded them yet.

## Upgrading

1. Read the release notes, especially anything about migrations.
2. Take a backup, as above.
3. Note the image digests you are running now (`docker compose images`).
4. Change the image reference in `compose.yaml` to the new version's digest,
   from its release page, and check it with `docker compose config`.
5. `docker compose pull`, then `docker compose run --rm server migrate`.
6. `docker compose up -d`, and check the server becomes healthy.

Migrations only ever move forward. Rolling back to the previous version is
possible only if it understands the migrated schema, which the release notes
say; otherwise roll back by restoring the backup from step 2 and running the
digests from step 3.

## What this deployment will not do

- run with `privileged: true`
- give the server container any device access
- leave a container its default capabilities or a writable root filesystem
  (`deploy/check-hardening.sh` checks a rendered stack; run it after editing
  the Compose files)
- pipe a remote script into a shell
- use a `latest` image tag
