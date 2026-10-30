# 使用 Docker Compose 部署与升级（Linux）

[English](deployment.md) · [README](../README.md)

## 服务器与浏览器分工

- **服务器（`cvd` 容器）**：采集交易所成交、补缺、把各数据源 K 线写入 PostgreSQL
  （待写入数据暂存 SQLite outbox），提供价格/基础流数据、`/meta`、`/health`、
  HTTP/WS 与网页静态文件，以及两个受信任的浏览器运行时模块（`/wasm-host/…`）。
- **浏览器**：所有指标（CVD、Flow、自定义/私有）都是用户在页面中导入的 `.wasm`
  文件，只在该浏览器本地运行。服务器不加载、不保存、不配置、不运行指标插件。

镜像在 Docker 内完成全部构建（Rust 服务端 + 受信任运行时 WASM）。服务器只需
Git 和带 Compose v2 的 Docker Engine；主机上不需要 Rust、Node、Chromium，也不需要
指标仓库。

`/indicators` 已退役（404），不能作为就绪检查。旧 `.so` 插件和
`[[indicators.library]]` 配置**不会**自动转换，只会被忽略。

---

## A. 升级现有部署（最常见）

前提（与你的环境不同请相应调整）：

- 部署目录是 `k0Iry/cvd-core` 的克隆；应用服务名为 `cvd`。
- 沿用你**现在**的 Compose 调用方式：相同的 `-f` 文件 / `COMPOSE_FILE`、相同的
  `-p` / `COMPOSE_PROJECT_NAME`、网络、反向代理、`.env`（密码）以及 PostgreSQL
  服务/卷。换了文件或项目名，可能会指向*另一组空的*卷（卷使用固定/external 名称时不一定）。
- outbox 在持久卷或绑定目录上（第 2 步检查）。

以下所有代码块请在**同一个 Bash 会话**中执行（它们共用变量）。先用 Bash 数组按你当前的
实际用法定义一次 Compose 调用：

```bash
DC=(docker compose)                                            # 使用仓库自带 compose.yaml
# DC=(docker compose -p myproj -f docker-compose.yml -f "my override.yml")  # 自定义：保持你原来的
# 若依赖 COMPOSE_FILE / COMPOSE_PROJECT_NAME 环境变量，请在本会话中保持其导出。
```

### 1. 检查代码目录——不要丢弃本地修改

```bash
cd /path/to/your/cvd-core
git remote -v                  # 必须指向 k0Iry/cvd-core（旧的 k0Iry/cvd 不是这个仓库）
git status --short             # 查看本地修改；不要盲目 reset/stash
"${DC[@]}" config --services   # 应列出你现有的服务（如 postgres、cvd）
```

如果 `origin` 是旧的 `k0Iry/cvd`，请停下：另行克隆 `cvd-core`，再有意识地迁移你自己的
Compose/配置/挂载，不要把另一套历史拉进旧目录。不要用模板覆盖现有 `.env` 或
`artifacts/cvd.toml`。

### 2. 备份（只备份，不删除、不迁移）

每次执行都会新建一个仅属主可读的备份目录和唯一的回滚镜像标签，不会覆盖以前的备份。
`cp` 中请列出你通过 `-f`/`COMPOSE_FILE` 选择的**每一个** Compose 文件以及实际配置路径。

```bash
set -euo pipefail
umask 077
BK="$HOME/cvd-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p -- "$HOME/cvd-backups"; mkdir -- "$BK"          # 目录已存在则失败
CID="$("${DC[@]}" ps -q cvd)"; test -n "$CID"
docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$CID" > "$BK/project"
ROLLBACK_IMAGE="cvd-core:rollback-$(basename "$BK")"
docker image tag "$(docker inspect -f '{{.Image}}' "$CID")" "$ROLLBACK_IMAGE"
printf '%s\n' "$ROLLBACK_IMAGE" > "$BK/rollback-image"
git rev-parse HEAD > "$BK/git-head"
cp -a -- .env compose.yaml artifacts/cvd.toml "$BK/"     # 自定义安装：换成你的 -f 文件和配置
echo "backup: $BK"
```

数据库按你原有方式备份。**仅限仓库自带模板**（服务 `postgres`、用户 `cvdadmin`、
数据库 `cvd`）：

```bash
set -euo pipefail
"${DC[@]}" exec -T postgres pg_dump -U cvdadmin -d cvd -Fc > "$BK/cvd.dump"
test -s "$BK/cvd.dump"
```

其他安装请替换为你自己的服务/用户/数据库名。

确认 outbox 已持久化——先查出它的**实际**路径，再看挂载：

```bash
"${DC[@]}" exec -T cvd sh -c 'if [ -n "${CVD_OUTBOX_PATH+x}" ]; then printf "%s\n" "$CVD_OUTBOX_PATH"; else echo UNSET; fi'
docker inspect -f 'workdir={{.Config.WorkingDir}}' "$CID"
docker inspect -f '{{range .Mounts}}{{.Destination}} {{.Type}} {{.Name}}{{.Source}}{{"\n"}}{{end}}' "$CID"
```

如果 `exec` 命令失败，请**停止**并先排查——失败不代表变量未设置。只有明确输出 `UNSET`
时才适用默认值 `data/outbox.sqlite`（仓库镜像已显式设置该变量）。
只打印这一个变量；不要输出容器的完整环境变量（其中含凭据）。相对路径以工作目录为基准。
outbox 所在目录必须等于或位于某个已列出的卷/绑定挂载的目标路径之下。仓库模板中是
`/app/data/outbox.sqlite`，由 `outbox-data` 卷挂在 `/app/data`——这只是示例，不是通用规则。
只有当没有任何挂载覆盖实际目录时，outbox 才位于容器可写层：此时在重建容器**之前**先按
[outbox 未持久化](#outbox-未持久化)处理。

### 3. 更新、构建、只重建 `cvd`

```bash
set -euo pipefail
git pull --ff-only
"${DC[@]}" config --quiet
"${DC[@]}" build cvd
"${DC[@]}" up -d --no-deps --no-build cvd
"${DC[@]}" logs --tail=100 cvd
```

`set -e` 使任何命令失败即停止。`git pull` 可能更新 `compose.yaml` 等受版本控制的文件；
升级前的副本在 `$BK` 中。PostgreSQL 保持运行，卷原样复用。**不要**执行 `down -v`、
删除卷、重启/替换 PostgreSQL，或让两个采集进程写同一份数据。

本次浏览器 WASM 改动不需要手动迁移或重算数据库、历史或游标。但服务每次启动仍会执行常规的
表结构初始化/兼容语句；较旧的部署可能因此发生表结构变化——这也是要先备份的原因之一。

如果配置里还有 `[indicators]` 段，日志会出现
`config [indicators] is retired and IGNORED …`，无害；方便时把文件精简为只剩
`[server]` 段（参考 `config/cvd.public.toml`），再重建一次 `cvd`。其他未知键仍会
阻止启动。旧 `.so` 文件/插件挂载已不再使用，可作为离线参考保留。

### 4. 验证

```bash
curl -fsS http://127.0.0.1:8080/health
curl -fsS http://127.0.0.1:8080/meta
curl -fsS http://127.0.0.1:8080/wasm-host/assets.json
```

若你发布的地址/端口不同请替换。查看 `/health` 的 JSON（预热期间可能短暂
`ready: false`；各数据源时间戳应持续前进）。然后打开网站并**强制刷新**
（Ctrl/Cmd+Shift+R），按 C 节导入指标。未导入指标时也应能看到价格和基础数据。

### 回滚

使用 `$BK` 中保存的镜像、Compose 文件和配置；卷保持原样（绝不删除）。以下为仓库模板布局的
示例——自定义安装请在 override 之前按原顺序加上每个保存的 `-f` 文件：

```bash
set -euo pipefail
BK="$HOME/cvd-backups/<时间戳>"          # 第 2 步打印的目录
printf 'services:\n  cvd:\n    image: %s\n    pull_policy: never\n' \
  "$(cat "$BK/rollback-image")" > "$BK/rollback.override.yml"
cp -a -- "$BK/cvd.toml" artifacts/cvd.toml
docker compose --project-directory "$PWD" --env-file "$BK/.env" -p "$(cat "$BK/project")" \
  -f "$BK/compose.yaml" -f "$BK/rollback.override.yml" up -d --no-deps --no-build cvd
```

`--env-file "$BK/.env"` 让 Compose 使用保存的 `.env`，而不是当前的。如果本次升级**新增**了
outbox 挂载（见“outbox 未持久化”），保存的 Compose 文件里没有它：必须把同一个卷/绑定加入
`rollback.override.yml`（或保留定义它的文件），回滚时绝不能让 outbox 回到容器可写层或丢弃已复制的目录。

override 并不会删除 `build:` 键；真正阻止构建的是 `--no-build`，因此会使用保存的镜像。
是否恢复数据库备份应作为单独、慎重的决定。签出旧提交再重新构建并不是经过验证的回滚方式。

### outbox 未持久化

仅当第 2 步发现**没有**挂载覆盖 outbox 的实际目录时执行。把 `OUTBOX_SRC` 设为已确认的
目录（模板示例：`/app/data`），`OUTBOX_DEST` 设为一个新的主机目录。先停止服务，再从
**已停止**的容器复制**整个目录**（含 `-wal`/`-shm`）。不要复制正在使用的 SQLite 文件；
不要 chown PostgreSQL 的数据目录。

```bash
set -euo pipefail
OUTBOX_SRC=/app/data             # 已确认的实际 outbox 所在目录
OUTBOX_DEST=/srv/cvd-outbox      # 必须是尚不存在的新目录
test ! -e "$OUTBOX_DEST"
"${DC[@]}" stop cvd              # 只停止；不要 rm/down 容器
OLD_CONTAINER="$("${DC[@]}" ps -aq cvd)"
test "$(docker inspect -f '{{.State.Running}}' "$OLD_CONTAINER")" = false
sudo mkdir -- "$OUTBOX_DEST"
sudo docker cp "$OLD_CONTAINER:$OUTBOX_SRC/." "$OUTBOX_DEST/"
sudo chown -R 10001:10001 "$OUTBOX_DEST"   # 容器以 UID/GID 10001 运行
```

在**你自己的** Compose 文件里给 `cvd` 加上 `- /srv/cvd-outbox:/app/data`
（即 `OUTBOX_DEST:OUTBOX_SRC`），再继续第 3 步。

---

## B. 全新安装（新的空数据库）

**会创建新卷。不要用于现有部署。**

```bash
set -euo pipefail
git clone https://github.com/k0Iry/cvd-core.git
cd cvd-core
cp .env.example .env && chmod 0600 .env
# 编辑 .env：POSTGRES_PASSWORD=<openssl rand -hex 24 的输出>
mkdir -p artifacts
install -m 0644 config/cvd.public.toml artifacts/cvd.toml
docker compose config --quiet
docker compose build cvd
docker compose up -d --no-build
docker compose logs --tail=100 cvd
curl -fsS http://127.0.0.1:8080/health
```

- `.env` 中 `LIVE_ACCESS_KEY`：留空 = 实时数据不限制；设置后实时数据需要该密钥
  （不是整站登录）。
- 之后修改 `POSTGRES_PASSWORD` 不会更改已初始化数据库的密码。
- 端口仅绑定 `127.0.0.1:8080`。远程访问：SSH 隧道
  （`ssh -L 8080:127.0.0.1:8080 user@server`）或用你的反向代理指向 `cvd:8080`。
- 持久数据：本 Compose 项目下的命名卷 `postgres-data` 与 `outbox-data`。升级时
  绝不要用 `docker compose down -v`。
- 以后升级：按 A 节，`DC=(docker compose)`。

---

## C. 在浏览器中使用指标（每个用户、每个浏览器）

1. 从公开的 [cvd-indicators Releases](https://github.com/k0Iry/cvd-indicators/releases)
   下载 `.wasm`（如 v0.1.0 的 CVD/Flow）并核对发布的 sha256；或使用你本地构建的
   自定义/私有 `.wasm`。导入本地文件不需要任何账号或 API 登录。
2. 打开网站 → **Indicators → Import** → 选择 `.wasm` → 勾选复选框启用。

安装、启用/停用、参数和卸载都只在浏览器本地进行：无需重启服务器、重新构建或改配置。
导入的模块按**来源（origin）**保存在该浏览器的 IndexedDB 中；换浏览器、换域名/端口
或清除站点数据后需要重新导入。文件不会上传到服务器。本地存储不是加密，也无法在页面
或设备被攻破时保护模块。

插件开发：[WASM 插件契约](plugin-wasm-v1.md) 与
[公开编写指南](https://github.com/k0Iry/cvd-indicators/blob/master/docs/authoring.md)。
原生 C-ABI runner（[plugin-api-v1.md](plugin-api-v1.md)）仅为历史文档。

## 许可

自有公开代码采用 Apache-2.0。请保留 LICENSE、NOTICE、第三方声明与 TradingView
署名。自定义/私有库遵循其自身条款。运营者需自行遵守所用行情数据源的条款。
