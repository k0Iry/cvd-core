# 部署与升级

[English](deployment.md) · [README](../README.md)

需要 **amd64 或 arm64 Linux** 和 **Docker Compose v2**。预构建镜像
`ghcr.io/k0iry/cvd-core:latest` 支持两种架构，无需编译源码或登录镜像仓库。

## 升级

在**原有的** Compose 目录操作；如果平时带有 `-f` / `-p` 参数，继续使用相同参数。
保留 `.env`、服务名、网络和 PostgreSQL/outbox 数据挂载。先备份配置和数据，
切勿运行 `docker compose down -v`。

```bash
cd /path/to/your/cvd-core &&
docker compose pull cvd &&
docker compose up -d --no-deps --no-build cvd &&
docker compose logs --tail=50 cvd
```

升级后，Coinbase USDC 合约会从已有的已收盘分钟历史自动回补，可能需要一段时间；
恢复期间工具栏显示 `Source recovering`，完成后显示 `Source live`。请保留数据库和
outbox，无需重置或新建分段。服务器除实时行情外，还需能访问公共 HTTPS
`history.deribit.com`。

<details>
<summary>仍在使用旧的源码构建或 TOML 配置？</summary>

只修改原有 Compose 文件中的 `cvd` 服务：

- 用 `image: ${CVD_IMAGE:-ghcr.io/k0iry/cvd-core:latest}` 替换
  `build:` / `pull_policy: never`。
- `cvd.toml` 和 `CVD_CONFIG` 已不再读取。如果旧 `[server]` 配置改过
  `http_addr` 或 `static_dir`，先将值移到 `CVD_HTTP_ADDR` 或 `CVD_STATIC_DIR`，
  再移除 `CVD_CONFIG` 和旧的配置/插件挂载。
- 执行上方升级命令，之后再删除旧配置文件；不要删除数据库或 outbox 存储。

</details>

## 首次安装

**仅限全新安装：**不要用这些文件覆盖已有部署。

```bash
mkdir cvd-core && cd cvd-core &&
base=https://raw.githubusercontent.com/k0Iry/cvd-core/master &&
curl -fsSLo compose.yaml "$base/compose.yaml" &&
curl -fsSLo .env "$base/.env.example" &&
chmod 600 .env
```

编辑 `.env`，将 `POSTGRES_PASSWORD` 设为足够长、适合放入 URL 的随机密码
（例如用 `openssl rand -hex 24` 生成），然后启动：

```bash
docker compose config --quiet &&
docker compose pull &&
docker compose up -d --no-build &&
docker compose logs --tail=50 cvd
```

无需应用配置文件。容器内默认监听 8080，静态文件目录为 `/app/static`；
可用 `CVD_HTTP_ADDR`、`CVD_STATIC_DIR` 覆盖。在 `.env` 中设置 `CVD_IMAGE`
可选择已发布的镜像标签，默认使用 `latest`。

## 打开应用

先在服务器上检查：

```bash
curl -fsS http://127.0.0.1:8080/health && echo OK
```

已有反向代理时，用原来的网址访问。否则在自己的电脑上运行以下命令并保持终端打开，
然后访问 `http://127.0.0.1:8080`：

```bash
ssh -N -L 8080:127.0.0.1:8080 user@server
```

升级后强制刷新页面（Ctrl+Shift+R / Cmd+Shift+R）。
指标操作见 [导入并启用](../README.md#use-indicators)。
