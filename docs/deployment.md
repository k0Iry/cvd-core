# Deploy and upgrade

[简体中文](deployment.zh-CN.md) · [README](../README.md)

You need a Linux **amd64 or arm64** server with **Docker Compose v2**. The app
runs from the prebuilt image `ghcr.io/k0iry/cvd-core` (published by CI from
`master`: tags `latest` and `sha-<full commit>`). Each tag is one multi-arch
image, natively built and tested for `linux/amd64` and `linux/arm64`; Docker
picks the matching one automatically (no `platform:` setting or login needed). Nothing is compiled on the
server; indicators are imported in the browser later.

Always keep your `.env` and Docker volumes (PostgreSQL
data and the outbox in `/app/data`). Never use `docker compose down -v` — it
deletes your data. If you normally run Compose with `-f` or `-p`, add the same
options to every `docker compose` command below.

## Configuration

There is no configuration file. The server listens on `0.0.0.0:8080` and serves
`static` relative to its working directory (`/app/static` in the image).
Optional overrides: `CVD_HTTP_ADDR` and `CVD_STATIC_DIR`. A bare binary run
outside the container now also defaults to port 8080 (previously 80).

`CVD_CONFIG` / `cvd.toml` are retired and ignored. If your Compose file still
mounts `artifacts/cvd.toml` (or a `/plugins` directory) into `cvd`:

1. If the old file set a non-default `[server] http_addr` or `static_dir`,
   copy those values into `CVD_HTTP_ADDR` / `CVD_STATIC_DIR` on the `cvd`
   service first; otherwise they are silently lost.
2. Remove the config volume entry and the `CVD_CONFIG` variable.
3. Run the upgrade commands below (`pull` then `up -d --no-deps --no-build cvd`);
   `up` alone may keep running the old image.
4. Only afterwards delete the old file, if you want to.

## Upgrade

```bash
cd /path/to/your/cvd-core &&
docker compose pull cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

### One time: switch an existing source-build install to the image

Back up `.env` and your data first. In your **existing**
directory, update the checkout with `git pull` (the new `compose.yaml` replaces
the `cvd` service's `build:`/`pull_policy: never` with the GHCR image; your
directory, project name, service names, volumes, untracked `.env`/config and
any `-f`/`COMPOSE_FILE` options stay the same), then run the upgrade commands above:

```bash
cd /path/to/your/cvd-core &&
git pull --ff-only &&
docker compose config --quiet
```

If you edited `compose.yaml` yourself, instead change only the `cvd` service:
remove `build:` and `pull_policy: never`, and set
`image: ${CVD_IMAGE:-ghcr.io/k0iry/cvd-core:latest}`.

To pin a version, set `CVD_IMAGE=ghcr.io/k0iry/cvd-core:sha-<full commit>`
in `.env`.

Maintainer note: after the first publish, set the GHCR package `cvd-core` to
**Public** once (and link it to this repository if not linked); afterwards
clients pull anonymously.

## First install (new server)

Only for a brand-new installation; don't run this over an existing one. This
downloads the deployment files once (no source checkout):

```bash
mkdir cvd-core && cd cvd-core &&
base=https://raw.githubusercontent.com/k0Iry/cvd-core/master &&
curl -fsSLo compose.yaml "$base/compose.yaml" &&
curl -fsSLo .env "$base/.env.example" &&
chmod 600 .env
```

Now edit `.env` and set `POSTGRES_PASSWORD` to a long random value
(for example the output of `openssl rand -hex 24`). Then start:

```bash
docker compose config --quiet &&
docker compose pull &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

Developers: to run a locally built image, `docker build -t cvd-core:local .`
and set `CVD_IMAGE=cvd-core:local`.

## Check it works

```bash
curl -fsS http://127.0.0.1:8080/health && echo OK
```

If you already use a reverse proxy, open your existing URL. On a remote server
without a browser, optionally run this on your own computer (replace
`user@server`, keep the terminal open), then open `http://127.0.0.1:8080` locally:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@server
```

After an upgrade, hard-refresh the page (Ctrl+Shift+R / Cmd+Shift+R).

## Use indicators

1. Download `cvd_indicator_cvd.wasm` and/or `cvd_indicator_flow.wasm` from the
   [cvd-indicators v0.1.0 release](https://github.com/k0Iry/cvd-indicators/releases/tag/v0.1.0).
   Your own `.wasm` files work too; no login is needed.
2. In the app, click **Indicators → Import**, choose the file and enable it.

The browser remembers indicators for this site. After clearing site data or
switching browser, import them again. Turning indicators on or off never needs a
server restart.
