# Deploy with Docker Compose (Linux)

[简体中文](deployment.zh-CN.md) · [README](../README.md)

The shortest public setup is **PostgreSQL + core + the public CVD/Flow libraries**.
No private repository, private key, registry account or CI run is required.
Builds happen locally on the target Docker host; no published image tag is assumed.

## Fresh installation

**This section creates new named database volumes. If you already run CVD, use
[Upgrade an existing deployment](#upgrade-an-existing-deployment) instead.**

Requirements: Git, Docker Engine with Compose v2+, and a Linux host with kernel
>=5.11. Docker/seccomp must allow `close_range(..., CLOSE_RANGE_CLOEXEC)`.
The build needs public GitHub/crate registry access; ingestion needs access to
the configured public exchange endpoints. Run these commands on the target
server so the host image and native libraries use the same architecture.
Stop on any command failure; do not continue to startup after a failed build.

### 1. Get source and configuration

```bash
git clone https://github.com/k0Iry/cvd-core.git
git clone https://github.com/k0Iry/cvd-indicators.git
# This public plugin revision is compatible with the current ABI v1 host.
git -C cvd-indicators checkout --detach 1e6673ba586bb76f1aaac96c433a68a9a41ecc96
cd cvd-core
cp .env.example .env
chmod 0600 .env
mkdir -p artifacts/plugins artifacts/public-build
install -m 0644 config/cvd.public.toml artifacts/cvd.toml
chmod 0755 artifacts/plugins
```

Edit `.env` and set **POSTGRES_PASSWORD** to a strong random URL-safe value
(hexadecimal avoids URI-encoding issues). Do not commit this file. Changing this
environment variable does **not** rotate the password in an already initialized
database. Leave LIVE_ACCESS_KEY empty for unrestricted real-time access, or set
it to require the key for real-time data. This is not general website authentication.

### 2. Build the public plugins locally

From the `cvd-core` directory:

```bash
docker run --rm \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -v cvd-public-cargo-cache:/cargo \
  -v "$PWD/../cvd-indicators:/src:ro" \
  -v "$PWD/artifacts/public-build:/target" \
  -w /src rust:1.96-slim-trixie \
  cargo build --locked --release -p cvd-indicator-cvd -p cvd-indicator-flow

install -m 0644 artifacts/public-build/release/libcvd_indicator_cvd.so artifacts/plugins/
install -m 0644 artifacts/public-build/release/libcvd_indicator_flow.so artifacts/plugins/
```

The sources are mounted read-only into a Rust 1.96 / Debian trixie builder,
matching the core image's toolchain/distribution. Only the two shared libraries
are installed into `artifacts/plugins`; build files stay outside that directory.
Do not copy a macOS library or a different-architecture `.so` onto this server.
The core Docker build excludes `artifacts`, including custom/private libraries.

### 3. Build and start

```bash
docker compose config --quiet
docker compose build cvd
docker compose up -d --no-build
docker compose logs --tail=100 cvd
```

Open **http://127.0.0.1:8080** on the server. For a remote machine, use an SSH
tunnel (`ssh -L 8080:127.0.0.1:8080 user@server`) or configure a reverse proxy.
The example binds to localhost rather than exposing an unauthenticated service
to the public Internet. PostgreSQL has no published host port.

### 4. Verify

```bash
curl -fsS http://127.0.0.1:8080/indicators
curl -fsS http://127.0.0.1:8080/health
```

- `/indicators`: `cvd` and `flow` should be `ready`, with `error: null`.
- `/health`: inspect the JSON, not just HTTP success. Startup warm-up can report
  `ready: false`; after recovery, check source timestamps advance and pending bars
  are being delivered. A temporarily nonzero `pending_bars` is possible during writes.
- Check that the chart displays price, CVD and Flow and continues receiving data.
  Plugin readiness alone does not prove exchange ingestion or browser rendering.

## What persists

The example [`compose.yaml`](../compose.yaml) creates two named volumes:

- `postgres-data`: the primary PostgreSQL history.
- `outbox-data`: a small SQLite queue of bars awaiting confirmed PostgreSQL writes,
  mounted at `/app/data`. The image initializes that directory for UID/GID 10001.

Ordinary container replacement preserves these named volumes. **Do not use
`docker compose down -v` for an upgrade.** Keep the same Compose project name;
a different project selects different named volumes. Maintain database backups.

## Configure, add or disable an indicator

Edit `artifacts/cvd.toml`, not the tracked example file. Paths are **container**
paths under `/plugins`. Each id must match the library's descriptor and be unique.
Use only trusted operator-provided native libraries; process isolation is not a
sandbox for untrusted code. See the [SDK contract](plugin-api-v1.md) and the
[public authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md).

For example, after separately building the public sample library, add:

```toml
[[indicators.library]]
id = "sample.channel"
path = "/plugins/libsample_indicator.so"
params = { length = 20 }
enabled = true
modes = ["source", "aggregate"]
```

To disable a configured library, set `enabled = false` **in its existing block**;
do not append a duplicate id. Re-enable with `true`. Apply configuration changes:

```bash
docker compose restart cvd
```

**No core rebuild is needed, but the application must restart.** Directory
watching, hot add/remove and runtime enable/disable APIs are not implemented.
The restart briefly interrupts ingestion; PostgreSQL and the outbox volumes remain.
Do not overwrite/truncate a loaded `.so` while the application is running. Build
to a staging directory first, stop `cvd`, install the new library, then start it.
Custom/private source and libraries must stay out of public Git commits and images;
`artifacts/` is excluded from this repository and the core Docker build context.

## Upgrade an existing deployment

Do not replace a working database service with this fresh-install example.
Keep your original database image/volume, credentials, Compose project name,
external networks and reverse-proxy setup. Keep the same user/HOME if existing
bind mounts use `${HOME}`. Old repository names do not identify the running image's
features; inspect the deployed version and mounts before deciding what to copy.

1. Back up the existing configuration/database and keep the old image for rollback.
   Build the replacement core image and libraries **before** stopping the old service.
2. Reuse the original DATABASE_URL. The plugin split introduces no plugin tables
   or required historical CVD recomputation. Normal startup compatibility DDL may
   still run; an older deployed version can differ from the split's source baseline.
3. Inspect the application's outbox separately from the PostgreSQL volume:
   - If the running version predates the outbox, create a new persistent application
     directory and let the new application initialize it.
   - If an outbox already exists, preserve its actual path and data. A missing file
     at one guessed path is not proof that no queue exists.
4. If it lives only in the old container, stop that application and copy the **whole
   SQLite directory**, including any WAL/SHM files, into a fresh host directory.
   Mount it at `/app/data`; give UID/GID 10001 write access to that application
   directory only. Never chown PostgreSQL's data directory to the application UID.
5. Install staged libraries and update the existing application's config/mounts.
   Recreate **only** the application using the original Compose project:
   `docker compose up -d --no-deps --no-build cvd`. Do not run two ingestion writers
   against the same store. If any step fails, stop and resolve it before proceeding.
6. Update a container-network reverse proxy to the application service's **8080**
   port (for example `cvd:8080` on a shared Docker network). `expose: 80` does not
   translate traffic to 8080. If the proxy runs in another stack, explicitly attach
   the application to its existing external network; this fresh example does not.
7. Verify `/indicators`, `/health` and the live chart before removing rollback assets.

For example, **only after confirming** that a stopped old container's outbox
is in `/app/data`, the copy operation is:

```bash
# Set OLD_CONTAINER to the confirmed old container and OUTBOX_DEST to a NEW directory.
(
  set -eu
  : "${OLD_CONTAINER:?set the old container}" "${OUTBOX_DEST:?set a fresh destination}"
  test ! -e "$OUTBOX_DEST" || { printf 'Destination already exists; refusing to overwrite.\n' >&2; exit 1; }
  test "$(docker inspect -f '{{.State.Running}}' "$OLD_CONTAINER")" = false
  mkdir -- "$OUTBOX_DEST"
  docker cp "$OLD_CONTAINER:/app/data/." "$OUTBOX_DEST/"
  test -f "$OUTBOX_DEST/outbox.sqlite"
  sudo chown -R 10001:10001 "$OUTBOX_DEST"
)
```

Only continue the cutover if this block succeeds. It refuses to overwrite an
existing directory and requires the old container to be stopped.

A normal restart retains a container's writable layer; deletion/recreation does not.
Preserve the outbox mount during future updates and rollback. A version without
outbox support will not consume a newer version's pending queue: drain it with a
supporting version or retain it for recovery, rather than discarding it.
Deleting an obsolete Git remote is different from deleting its old deployment
folder, which may now contain the active Compose/config/plugin mounts.

## Failure boundaries and verification policy

- Runner crashes/timeouts make that indicator unavailable; they do not turn missing
  output into zero. The runner does not own PostgreSQL persistence or ingestion.
- Linux >=5.11 and a compatible seccomp policy are required for descriptor isolation.
  If `close_range` is denied, spawning fails closed. Do not solve this by blindly
  granting privileged mode or loading unknown libraries.
- Diagnose config/path/architecture/library errors in application logs. Keep raw
  private diagnostics private; public API errors are intentionally bounded.
- Local tests and release builds precede any approved final CI build. The workflow
  is manually dispatched; these deployment commands do not trigger CI or publish
  images. Configuration validation, image construction and live acceptance are
  separate checks, not interchangeable success claims.

## Licenses

Owned public code is Apache-2.0. Retain LICENSE, NOTICE, third-party notices and
TradingView attribution. Custom/private libraries retain their own terms.
Operators are responsible for the terms governing their market-data feeds.
