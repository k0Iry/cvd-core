# Deploy and upgrade with Docker Compose (Linux)

[简体中文](deployment.zh-CN.md) · [README](../README.md)

## What runs where

- **Server (`cvd` container):** collects exchange trades, recovers gaps, stores
  per-source bars in PostgreSQL (SQLite outbox for pending writes), serves
  price/base flow data, `/meta`, `/health`, HTTP/WS and the static web app,
  including the two trusted browser runtime modules (`/wasm-host/…`).
- **Browser:** every indicator (CVD, Flow, custom/private) is a `.wasm` file that
  the visitor imports in the page. It runs locally in that browser. The server
  does not load, store, configure or run indicator plugins.

The image builds everything it needs (Rust server + trusted runtime WASM) inside
Docker. The server host needs only Git and Docker Engine with Compose v2; no host
Rust, Node or Chromium, and no indicator repository on the server.

`/indicators` is retired (404) and is not a readiness check. Old `.so` plugins and
`[[indicators.library]]` config are **not** converted; they are ignored.

---

## A. Upgrade an existing deployment (most common)

Assumptions — adjust if yours differ:

- The deployment checkout is a clone of `k0Iry/cvd-core`; the app service is `cvd`.
- You keep **the same** Compose invocation you already use: the same `-f` files /
  `COMPOSE_FILE`, the same `-p` / `COMPOSE_PROJECT_NAME`, networks, reverse proxy,
  `.env` (password) and PostgreSQL service/volume. A different file or project
  name can select *different, empty* volumes (unless they use fixed/external names).
- The outbox is on a persistent volume or bind mount (checked in step 2).

Run every block below in **one Bash session** (they share variables). Define your
invocation once as a Bash array, exactly as you run Compose today:

```bash
DC=(docker compose)                                            # stock compose.yaml
# DC=(docker compose -p myproj -f docker-compose.yml -f "my override.yml")  # custom: keep yours
# If you rely on COMPOSE_FILE / COMPOSE_PROJECT_NAME, keep them exported in this shell.
```

### 1. Check the checkout — do not discard local changes

```bash
cd /path/to/your/cvd-core
git remote -v                  # must point to k0Iry/cvd-core (old k0Iry/cvd is NOT this repo)
git status --short             # review local edits; do not reset/stash them away blindly
"${DC[@]}" config --services   # must list your existing services (e.g. postgres, cvd)
```

If `origin` is the old `k0Iry/cvd` repository, stop: clone `cvd-core` separately
and port your own Compose/config/mounts deliberately rather than pulling a
different history into the old folder. Never overwrite your existing `.env` or
`artifacts/cvd.toml` with the template files.

### 2. Back up (backup only — nothing is deleted or migrated)

Each run creates a **new** owner-only backup directory and a unique rollback
image tag, so earlier backups are never overwritten. List **every** Compose file
you select with `-f`/`COMPOSE_FILE` and your real config path in `cp`.

```bash
set -euo pipefail
umask 077
BK="$HOME/cvd-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -- "$HOME/cvd-backups"; mkdir -- "$BK"          # fails if it already exists
CID="$("${DC[@]}" ps -q cvd)"; test -n "$CID"
docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$CID" > "$BK/project"
ROLLBACK_IMAGE="cvd-core:rollback-$(basename "$BK")"
docker image tag "$(docker inspect -f '{{.Image}}' "$CID")" "$ROLLBACK_IMAGE"
printf '%s\n' "$ROLLBACK_IMAGE" > "$BK/rollback-image"
git rev-parse HEAD > "$BK/git-head"
cp -a -- .env compose.yaml artifacts/cvd.toml "$BK/"     # custom: your -f files + config
echo "backup: $BK"
```

Back up the database with your usual scheme. For the **stock template only**
(service `postgres`, user `cvdadmin`, database `cvd`):

```bash
set -euo pipefail
"${DC[@]}" exec -T postgres pg_dump -U cvdadmin -d cvd -Fc > "$BK/cvd.dump"
test -s "$BK/cvd.dump"
```

Other installations: substitute your own service/user/database names.

Check the outbox is persistent — find its **actual** path, then the mounts:

```bash
"${DC[@]}" exec -T cvd sh -c 'if [ -n "${CVD_OUTBOX_PATH+x}" ]; then printf "%s\n" "$CVD_OUTBOX_PATH"; else echo UNSET; fi'
docker inspect -f 'workdir={{.Config.WorkingDir}}' "$CID"
docker inspect -f '{{range .Mounts}}{{.Destination}} {{.Type}} {{.Name}}{{.Source}}{{"\n"}}{{end}}' "$CID"
```

If the `exec` command fails, **stop** and fix that first: a failure does not mean
the variable is unset. Only an explicit `UNSET` means the default
`data/outbox.sqlite` applies (the stock image sets the variable explicitly).
Print only that one variable; do not dump the container's full environment
(it contains credentials). A relative path is relative to the working directory.
The outbox's directory must be equal to, or below, the destination of a listed
volume/bind. In the stock template that is `/app/data/outbox.sqlite` under the
`outbox-data` volume at `/app/data` — an example, not a universal rule. Only if no
mount covers the real directory does the outbox live in the container's writable
layer: then follow [Outbox not persistent](#outbox-not-persistent) **before**
recreating the container.

### 3. Update, build, recreate only `cvd`

```bash
set -euo pipefail
git pull --ff-only
"${DC[@]}" config --quiet
"${DC[@]}" build cvd
"${DC[@]}" up -d --no-deps --no-build cvd
"${DC[@]}" logs --tail=100 cvd
```

`set -e` stops at the first failing command. `git pull` may update tracked files
such as `compose.yaml`; your pre-upgrade copies are in `$BK`. PostgreSQL keeps
running and volumes are reused. Do **not** run `down -v`, remove volumes,
restart/replace PostgreSQL, or run two collectors against the same data.

The browser-WASM change needs no manual database, history or cursor migration or
recompute. As on every start, the server still runs its normal schema
initialization/compatibility statements; an older deployment may receive such
changes, which is one reason to take the backup first.

If your config still has `[indicators]` blocks, the log shows
`config [indicators] is retired and IGNORED …`. It is harmless; when convenient,
reduce the file to the `[server]` section only (see `config/cvd.public.toml`) and
recreate `cvd` again. Other unknown keys still stop startup. Old `.so` files and
plugin mounts are unused; keep them as offline references if you like.

### 4. Verify

```bash
curl -fsS http://127.0.0.1:8080/health
curl -fsS http://127.0.0.1:8080/meta
curl -fsS http://127.0.0.1:8080/wasm-host/assets.json
```

Use your own address/port if you publish differently. Inspect the `/health` JSON
(warm-up may briefly report `ready: false`; source timestamps should advance).
Then open the site, **hard-refresh** (Ctrl/Cmd+Shift+R) and import indicators
(section C). Price and base data should display before any indicator is imported.

### Rollback

Use the saved image, Compose files and config from `$BK`; volumes stay as they
are (never delete them). Example for the stock layout — for a custom install add
each saved `-f` file in the original order before the override:

```bash
set -euo pipefail
BK="$HOME/cvd-backups/<timestamp>"       # the directory printed in step 2
printf 'services:\n  cvd:\n    image: %s\n    pull_policy: never\n' \
  "$(cat "$BK/rollback-image")" > "$BK/rollback.override.yml"
cp -a -- "$BK/cvd.toml" artifacts/cvd.toml
docker compose --project-directory "$PWD" --env-file "$BK/.env" -p "$(cat "$BK/project")" \
  -f "$BK/compose.yaml" -f "$BK/rollback.override.yml" up -d --no-deps --no-build cvd
```

`--env-file "$BK/.env"` makes Compose use the saved `.env`, not today's. If this
upgrade added a **new** outbox mount (see "Outbox not persistent"), the saved
Compose files lack it: add that same volume/bind to `rollback.override.yml` (or
keep the file that defines it), so rollback never returns the outbox to the
container's writable layer or abandons the copied directory.

The override does not remove the `build:` key; `--no-build` is what prevents a
build, so the saved image is used. Restore the database dump only as a deliberate,
separate decision. Checking out an old commit and rebuilding is not a verified
rollback.

### Outbox not persistent

Only if step 2 found **no** mount covering the real outbox directory. Set
`OUTBOX_SRC` to that confirmed directory (stock example: `/app/data`) and
`OUTBOX_DEST` to a new host directory. The service is stopped first and the
**whole directory** (including `-wal`/`-shm`) is copied from the **stopped**
container. Never copy a live SQLite file; never chown PostgreSQL's data.

```bash
set -euo pipefail
OUTBOX_SRC=/app/data             # confirmed parent directory of your real outbox
OUTBOX_DEST=/srv/cvd-outbox      # must NOT exist yet
test ! -e "$OUTBOX_DEST"
"${DC[@]}" stop cvd              # stop only; do not rm/down the container
OLD_CONTAINER="$("${DC[@]}" ps -aq cvd)"
test "$(docker inspect -f '{{.State.Running}}' "$OLD_CONTAINER")" = false
sudo mkdir -- "$OUTBOX_DEST"
sudo docker cp "$OLD_CONTAINER:$OUTBOX_SRC/." "$OUTBOX_DEST/"
sudo chown -R 10001:10001 "$OUTBOX_DEST"   # container runs as UID/GID 10001
```

Add `- /srv/cvd-outbox:/app/data` (your `OUTBOX_DEST:OUTBOX_SRC`) to the `cvd`
service in **your** Compose file, then continue with step 3.

---

## B. Fresh installation (new, empty database)

**Creates new volumes. Do not use for an existing deployment.**

```bash
set -euo pipefail
git clone https://github.com/k0Iry/cvd-core.git
cd cvd-core
cp .env.example .env && chmod 0600 .env
# edit .env: POSTGRES_PASSWORD=<output of: openssl rand -hex 24>
mkdir -p artifacts
install -m 0644 config/cvd.public.toml artifacts/cvd.toml
docker compose config --quiet
docker compose build cvd
docker compose up -d --no-build
docker compose logs --tail=100 cvd
curl -fsS http://127.0.0.1:8080/health
```

- `LIVE_ACCESS_KEY` in `.env`: empty = unrestricted real-time data; set = key
  required for real-time data (not site-wide authentication).
- Changing `POSTGRES_PASSWORD` later does not rotate an initialized database's
  password.
- The port is bound to `127.0.0.1:8080`. Remote access: SSH tunnel
  (`ssh -L 8080:127.0.0.1:8080 user@server`) or your reverse proxy to `cvd:8080`.
- Persistent data: named volumes `postgres-data` and `outbox-data` under this
  Compose project. Never use `docker compose down -v` for upgrades.
- Later upgrades: section A with `DC=(docker compose)`.

---

## C. Indicators in the browser (each user, each browser)

1. Download `.wasm` assets from the public
   [cvd-indicators Releases](https://github.com/k0Iry/cvd-indicators/releases)
   (e.g. v0.1.0 CVD/Flow) and check the published sha256; or use your own
   locally built custom/private `.wasm`. No account or API login is needed to
   import a local file.
2. Open the site → **Indicators → Import** → choose the `.wasm` → tick its
   checkbox to enable.

Install, enable/disable, parameters and uninstall are browser-local: no server
restart, rebuild or config change. Imported modules are stored in that browser's
IndexedDB **per origin**; a different browser, a changed domain/port, or cleared
site data needs re-import. Files are not uploaded to the server. Local storage is
not encryption and cannot protect a module from a compromised page or device.

Plugin authoring: [WASM plugin contract](plugin-wasm-v1.md) and the
[public authoring guide](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md).
The native C-ABI runner ([plugin-api-v1.md](plugin-api-v1.md)) is historical only.

## Licenses

Owned public code is Apache-2.0. Retain LICENSE, NOTICE, third-party notices and
TradingView attribution. Custom/private libraries retain their own terms.
Operators are responsible for the terms governing their market-data feeds.
