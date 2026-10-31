# 部署与升级

[English](deployment.md) · [README](../README.md)

服务器需要 **x86-64（amd64）Linux** 和 **Docker Compose v2**。应用直接运行预构建镜像
`ghcr.io/k0iry/cvd-core`（由 CI 从 `master` 发布，标签为 `latest` 和
`sha-<完整提交>`）。服务器上不编译任何东西；指标之后在浏览器里导入。

始终保留 `.env`、`artifacts/cvd.toml` 和 Docker 卷（PostgreSQL 数据与
`/app/data` 中的 outbox）。切勿使用 `docker compose down -v`——它会删除数据。
如果你平时运行 Compose 带有 `-f` 或 `-p` 参数，请在下面每条 `docker compose`
命令里加上同样的参数。

## 升级

```bash
cd /path/to/your/cvd-core &&
docker compose pull cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

### 一次性：把原来的源码构建部署切换到镜像

先备份 `.env`、`artifacts/cvd.toml` 和数据。在你**原有的**目录中用 `git pull` 更新
代码检出（新的 `compose.yaml` 把 `cvd` 服务的 `build:`/`pull_policy: never` 换成
GHCR 镜像；目录、项目名、服务名、卷、未跟踪的 `.env`/配置以及 `-f`/`COMPOSE_FILE`
选项都不变），然后执行上面的升级命令：

```bash
cd /path/to/your/cvd-core &&
git pull --ff-only &&
docker compose config --quiet
```

如果你自己改过 `compose.yaml`，则只修改 `cvd` 服务：删除 `build:` 和
`pull_policy: never`，并设置 `image: ${CVD_IMAGE:-ghcr.io/k0iry/cvd-core:latest}`。

如需固定版本，在 `.env` 中设置 `CVD_IMAGE=ghcr.io/k0iry/cvd-core:sha-<完整提交>`。

维护者注意：首次发布后，需一次性把 GHCR 包 `cvd-core` 设为 **Public**（如未关联则关联到本仓库）；
之后客户端可匿名拉取。

## 首次安装（新服务器）

仅用于全新安装，不要在已有部署上运行。下面只一次性下载部署文件（无需源码）：

```bash
mkdir cvd-core && cd cvd-core && mkdir -p artifacts &&
base=https://raw.githubusercontent.com/k0Iry/cvd-core/master &&
curl -fsSLo compose.yaml "$base/compose.yaml" &&
curl -fsSLo .env "$base/.env.example" &&
curl -fsSLo artifacts/cvd.toml "$base/config/cvd.public.toml" &&
chmod 600 .env
```

现在编辑 `.env`，把 `POSTGRES_PASSWORD` 设为一个足够长的随机值
（例如 `openssl rand -hex 24` 的输出）。然后启动：

```bash
docker compose config --quiet &&
docker compose pull &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

开发者：如需运行本地构建的镜像，执行 `docker build -t cvd-core:local .`，
并设置 `CVD_IMAGE=cvd-core:local`。

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
