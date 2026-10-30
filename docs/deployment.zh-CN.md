# 部署与升级

[English](deployment.md) · [README](../README.md)

服务器只需要 **Git** 和 **Docker Compose v2**。服务器上只放本仓库（`cvd-core`）；
指标之后在浏览器里导入即可。

## 升级已有部署

开始前，请先备份配置（`.env`、`artifacts/cvd.toml`）和数据，并确认 PostgreSQL
数据和 outbox 是持久化的（outbox 默认目录为 `/app/data`；如果改过，请检查实际路径）。
如果不确定数据存在哪里，请先检查清楚——替换容器时不能丢失这些数据。

在你**原有的** `cvd-core` 目录中运行：

```bash
cd /path/to/your/cvd-core &&
git pull --ff-only &&
docker compose config --quiet &&
docker compose build cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

如果你平时运行 Compose 时带有 `-f` 或 `-p` 参数，请在上面每条 `docker compose`
命令里加上同样的参数。

保留现有的 `.env` 和 `artifacts/cvd.toml`，不要用示例文件覆盖它们。
切勿使用 `docker compose down -v`——它会删除你的数据。

## 首次安装（新服务器）

仅用于全新安装，不要在已有部署上运行。

```bash
git clone https://github.com/k0Iry/cvd-core.git &&
cd cvd-core &&
cp .env.example .env &&
chmod 600 .env &&
mkdir -p artifacts &&
cp config/cvd.public.toml artifacts/cvd.toml
```

现在编辑 `.env`，把 `POSTGRES_PASSWORD` 设为一个足够长的随机值
（例如 `openssl rand -hex 24` 的输出）。然后启动：

```bash
docker compose config --quiet &&
docker compose build cvd &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

## 检查是否正常运行

```bash
curl -fsS http://127.0.0.1:8080/health && echo OK
```

如果已有反向代理，直接用原来的网址访问。远程服务器上没有浏览器时，可在你自己的电脑上
运行以下命令（替换 `user@server`，保持终端开着），然后在本机打开 `http://127.0.0.1:8080`：

```bash
ssh -N -L 8080:127.0.0.1:8080 user@server
```

升级后请强制刷新页面（Ctrl+Shift+R / Cmd+Shift+R）。

## 使用指标

1. 从 [cvd-indicators v0.1.0 发布页](https://github.com/k0Iry/cvd-indicators/releases/tag/v0.1.0)
   下载 `cvd_indicator_cvd.wasm` 和/或 `cvd_indicator_flow.wasm`。
   你自己的 `.wasm` 文件同样可用，无需登录。
2. 在应用中点击 **Indicators → Import**，选择文件并启用。

浏览器会按站点记住已导入的指标。清除站点数据或换浏览器后，需要重新导入。
开关指标从不需要重启服务器。
