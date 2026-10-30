# Deploy and upgrade

[简体中文](deployment.zh-CN.md) · [README](../README.md)

You need a Linux server with **Git** and **Docker Compose v2**. Only this
repository (`cvd-core`) goes on the server; indicators are imported in the
browser later.

## Upgrade an existing installation

Before you start, back up your configuration (`.env`, `artifacts/cvd.toml`) and
data, and make sure PostgreSQL data and the outbox are persistent (the default
outbox directory is `/app/data`; if you changed it, check your actual path).
If you are unsure where your data lives, check that first — replacing the
container must not lose it.

Run this in your **existing** `cvd-core` directory:

```bash
cd /path/to/your/cvd-core &&
git pull --ff-only &&
docker compose config --quiet &&
docker compose build cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

If you normally run Compose with `-f` or `-p` options, add the same options to
every `docker compose` command above.

Keep your existing `.env` and `artifacts/cvd.toml`; don't replace them with the
example files. Never use `docker compose down -v` — it deletes your data.

## First install (new server)

Only for a brand-new installation; don't run this over an existing one.

```bash
git clone https://github.com/k0Iry/cvd-core.git &&
cd cvd-core &&
cp .env.example .env &&
chmod 600 .env &&
mkdir -p artifacts &&
cp config/cvd.public.toml artifacts/cvd.toml
```

Now edit `.env` and set `POSTGRES_PASSWORD` to a long random value
(for example the output of `openssl rand -hex 24`). Then start:

```bash
docker compose config --quiet &&
docker compose build cvd &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

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
