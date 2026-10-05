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
docker compose -f deploy/compose.yaml config    # read the rendered output
docker compose -f deploy/compose.yaml up -d
```

Review the rendered configuration before starting: image references, volume
mappings, ports, and the env file. Pin an image digest rather than a tag for a
real deployment.

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
RomM's folder bind-mounted, as above, they are copied, so allow the space. ISO
and CUE/BIN images are exported; a disc imported only as a cdrdao TOC is not,
because RomM's emulators do not read that format.

## Verifying a release

Each release is built by `.github/workflows/release.yml` from its tag, and
signed by that workflow with a short-lived Sigstore certificate: there is no
long-lived key to steal, and a signature proves which workflow, at which tag,
in which repository, built the bytes. The release page gives the image's
digest. Use the digest, not the tag, in `compose.yaml`; a tag can be moved,
a digest cannot.

With [cosign](https://docs.sigstore.dev/cosign/system_config/installation/)
installed from your distribution or its release page:

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
version of everything in the image: publishing the image conveys object code
for GPL programs such as xorriso and cdrdao, and that is where their source
is.

## What this deployment will not do

- run with `privileged: true`
- give the server container any device access
- leave a container its default capabilities or a writable root filesystem
  (`deploy/check-hardening.sh` checks a rendered stack; run it after editing
  the Compose files)
- pipe a remote script into a shell
- use a `latest` image tag
