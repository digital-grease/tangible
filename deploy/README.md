# Deployment

Three Compose files. Combine them; do not edit them in place.

| File | Contains | Device access |
|---|---|---|
| `compose.yaml` | server, PostgreSQL | none |
| `compose.dev.yaml` | development overrides, local build, exposed database | none |
| `compose.hardware.yaml` | one burn worker per optical drive | `/dev/srN` + `/dev/sgN` |

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
ls -l /dev/sr* /dev/sg*          # identify the pair for your drive
# set TANGIBLE_BLOCK_DEVICE, TANGIBLE_SCSI_DEVICE, TANGIBLE_WORKER_NAME in .env

docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml config
# read the rendered output, confirm the device mappings are the drive you meant
docker compose -f deploy/compose.yaml -f deploy/compose.hardware.yaml up -d
```

One worker per drive. To add a second, copy the `burner-sr0` service, rename
it, and map that drive's own device pair.

## Why two device nodes

The block device carries data. The generic SCSI device carries the commands
that query drive capabilities, inspect the medium, and control writing. Both
are required.

## What this deployment will not do

- run with `privileged: true`
- give the server container any device access
- pipe a remote script into a shell
- use a `latest` image tag
