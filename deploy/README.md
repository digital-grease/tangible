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

## Production

```bash
cp deploy/.env.example deploy/.env
docker compose -f deploy/compose.yaml config    # read the rendered output
docker compose -f deploy/compose.yaml up -d
```

Review the rendered configuration before starting: image references, volume
mappings, ports, and the env file. Pin an image digest rather than a tag for a
real deployment.

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

## What this deployment will not do

- run with `privileged: true`
- give the server container any device access
- pipe a remote script into a shell
- use a `latest` image tag
