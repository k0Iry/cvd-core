# Deploy and upgrade

[简体中文](deployment.zh-CN.md) · [README](../README.md)

You need Linux **amd64 or arm64** and **Docker Compose v2**. The prebuilt image
`ghcr.io/k0iry/cvd-core:latest` supports both architectures; no source build or
registry login is needed.

## Upgrade

Use your **existing** Compose directory and the same `-f` / `-p` options, if any.
Keep your `.env`, service names, networks and PostgreSQL/outbox mounts. Back up
your configuration and data first; never run `docker compose down -v`.

```bash
cd /path/to/your/cvd-core &&
docker compose pull cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

<details>
<summary>Upgrading an older source-build or TOML installation?</summary>

Change only the `cvd` service in your existing Compose file:

- Replace `build:` / `pull_policy: never` with
  `image: ${CVD_IMAGE:-ghcr.io/k0iry/cvd-core:latest}`.
- `cvd.toml` and `CVD_CONFIG` are no longer read. If the old `[server]` section
  changed `http_addr` or `static_dir`, move those values to `CVD_HTTP_ADDR` or
  `CVD_STATIC_DIR` first. Then remove `CVD_CONFIG` and the old config/plugin mounts.
- Run the upgrade commands above. Only afterwards remove obsolete config files;
  do not remove database or outbox storage.

</details>

## First install

**New installations only:** do not download these files over an existing deployment.

```bash
mkdir cvd-core && cd cvd-core &&
base=https://raw.githubusercontent.com/k0Iry/cvd-core/master &&
curl -fsSLo compose.yaml "$base/compose.yaml" &&
curl -fsSLo .env "$base/.env.example" &&
chmod 600 .env
```

Edit `.env` and set `POSTGRES_PASSWORD` to a long, URL-safe random password
(for example, generate one with `openssl rand -hex 24`). Then start:

```bash
docker compose config --quiet &&
docker compose pull &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

No application config file is needed. The container listens on port 8080 and
serves `/app/static`. Optional overrides are `CVD_HTTP_ADDR` and `CVD_STATIC_DIR`;
`CVD_IMAGE` in `.env` selects a published image tag instead of `latest`.

## Open the app

Check locally on the server:

```bash
curl -fsS http://127.0.0.1:8080/health && echo OK
```

Use your existing reverse-proxy URL. Otherwise, run this on your computer and
keep it open, then visit `http://127.0.0.1:8080`:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@server
```

After an upgrade, hard-refresh the page (Ctrl+Shift+R / Cmd+Shift+R).
For indicators, follow [Import and enable](../README.md#use-indicators).
