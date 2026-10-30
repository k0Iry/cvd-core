# Docker Compose 部署（Linux）

[English](deployment.md) · [返回 README](../README.md)

公开版只需要 **PostgreSQL + core + CVD／Flow 两个公开插件**。不需要私有仓库、
注册表账号或运行 CI。下面在目标服务器本地构建，不假定已有可拉取的新镜像。

> 以下是**全新安装**。已有数据库的部署不要直接套用新数据卷；见文末升级说明。

## 升级：浏览器本地指标（服务端 runner 已退役）

服务端不再运行指标插件，只负责采集/恢复/持久化并提供标准基础数据（含权威累计 CVD）。
不需要也不会做数据库、历史、游标或 outbox 迁移。完整命令见英文版 docs/deployment.md
“Upgrade: browser-local indicators”：pg_dump 备份 → checkout 发布版本 → `docker compose build cvd`
→ `docker compose up -d --no-deps cvd` → 日志应出现 “config [indicators] is retired and IGNORED”。
旧 `[indicators]` 配置会被接受但不加载，并逐个库打印警告；其他未知键仍会拒绝启动。
用户在浏览器 Indicators → Import 导入从 Release 下载并核对 sha256 的 `.wasm`，仅保存在本浏览器。
以下为旧版说明，仅供参考。


## 1. 获取源码和配置

要求：Git、Docker Engine、Compose v2+、Linux >=5.11，Docker/seccomp 允许
close_range(..., CLOSE_RANGE_CLOEXEC)。在目标机器上构建，避免 .so 架构不匹配。
每一步失败就停止，不继续启动。构建需要访问公开 GitHub／crate registry，
采集需要访问交易所公开接口。

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

编辑 `.env`，给 `POSTGRES_PASSWORD` 填入足够长的随机 URL 安全密码（十六进制
可以避免连接串转义问题）。不要提交 `.env`。已有数据库的密码不会因为修改
这个环境变量而自动改变。

`LIVE_ACCESS_KEY` 留空表示实时数据不设密钥限制；设置后会限制实时访问，
但它不是整个网站的登录认证。

## 2. 构建公开插件

继续在 cvd-core 目录执行：

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

构建环境与 core 都是 Rust 1.96／Debian trixie，源码只读挂载。
plugins 目录里只放两份库，不包含源码和编译中间文件；不能直接使用其他系统／
架构生成的库。`artifacts/` 不进入公开 Git 或 core 镜像构建上下文。

## 3. 构建并启动应用

```bash
docker compose config --quiet
docker compose build cvd
docker compose up -d --no-build
docker compose logs --tail=100 cvd
```

本机打开 **http://127.0.0.1:8080**。远程服务器可用
`ssh -L 8080:127.0.0.1:8080 user@server` 转发后访问，或配置反向代理。
示例只绑定 localhost，不直接暴露公网；PostgreSQL 不开放宿主机端口。

## 4. 检查

```bash
curl -fsS http://127.0.0.1:8080/indicators
curl -fsS http://127.0.0.1:8080/health
```

- `/indicators` 中 cvd、flow 应为 ready，error 为 null。
- `/health` 要看 JSON，而不只看 HTTP 成功。等预热完成后，检查 ready、来源时间
  是否推进，以及 pending_bars 是否正常消化；写入中可能短暂有待写记录。
- 打开图表确认价格、CVD、Flow 显示并继续更新。插件 ready 不等于已验证行情链路。

## 日常操作

配置文件是 `artifacts/cvd.toml`，库文件是 `artifacts/plugins/*.so`。
新增插件要增加配置段；禁用时在**已有配置段**设置 `enabled = false`，启用为 true。
修改配置后执行：

```bash
docker compose restart cvd
```

**无需重编 core，但仍需重启应用。当前没有热加载／热禁用 API。**
不要运行中直接覆盖正在加载的 .so；先编译到临时构建目录，再停 cvd、安装库、启动。
重启会短暂打断采集，不需要重启 PostgreSQL。

只有运营者信任的 native 库才能加载；进程隔离不是不可信代码的安全沙箱。
自己的私有源码／库不要提交到公开仓库或烘焙进公开镜像。

## 已有部署如何升级

**保留原数据库服务／数据卷、密码、Compose 项目名及反代网络。** 不要因本仓库
提供了新安装示例就创建一个空库。使用 `${HOME}` 绑定路径时也保持原用户／HOME。

1. 先保留旧镜像与配置、按现有方案备份数据库；旧服务运行期间把新镜像／库编好。
2. 应用沿用原 DATABASE_URL。插件化没有新增插件表，也不要求重算历史 CVD；
   启动仍有兼容性建表／补列逻辑，较老线上版本可能与拆分基线不同。
3. 检查**实际线上版本**是否已有 outbox，不要从“旧仓库”或“Compose 没写”来猜。
   没有则新建持久化应用目录；有则保留真实文件，默认路径不存在不代表没有队列。
4. 如果 outbox 在旧容器内部：停旧应用后复制完整 SQLite 目录（包括 WAL／SHM），
   挂到新版 `/app/data`，只将应用目录赋予 UID/GID 10001 写权限。
   **不要修改 PostgreSQL 数据目录的属主。**
5. 在原 Compose 中改应用镜像、配置和挂载，只替换应用：
   `docker compose up -d --no-deps --no-build cvd`。不要同时运行两套采集器。
6. 容器网络反代上游改到应用的 **8080** 端口。expose 不是端口转换；不同 Compose
   栈之间还需把应用接入原来的 external 反代网络。
7. 检查指标、健康状态和图表，再清理回退材料。

普通容器 restart 保留可写层，删除重建不保留。新示例用两个 named volume 分别
保存 PostgreSQL 和 outbox；已有部署也可以保留原 bind mount。不要在升级时
执行 `docker compose down -v`，也不要改变项目名而误选新卷。

回退时保留 outbox 挂载。如果旧程序不支持 outbox，它不会处理新版留下的待写队列；
应先用支持的版本排空或保留文件以便恢复，不能直接丢弃。
删除旧 GitHub 仓库也不等于能删旧部署目录：新配置和插件可能还在该目录内。

更详细的目录复制示例、插件配置和故障边界见 [英文指南](deployment.md)。
所有检查先在本地完成；CI 仅手动触发，本指南不会运行 CI 或发布镜像。
