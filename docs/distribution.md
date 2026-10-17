# 安装二进制与接入项目

Shipbraid 的用户入口是发行包中的 `bin/shipbraid`，同一个程序提供服务端和 CLI。Dashboard 已构建在 `web/`，运行时无需 Rust、Cargo、Node.js 或 pnpm。源码构建只供维护者和贡献者使用。

首版发行目标为 **Linux x86_64 GNU**。官方 workflow 使用 Ubuntu 22.04 构建；需要兼容该构建环境的 glibc（2.35+），其他发行版仍需验收。CLI 不需要 Python/systemd；下面的安装器、适配器和 GitHub 接收器需要 Python **3.11+**。本机部署适配器额外需要 Git、systemd **249+** user manager 和 cgroup v2。需要 bash/python3 的项目脚本应自行具备其依赖。

## 下载与检查

从 [GitHub Releases](https://github.com/aak1247/shipbraid/releases) 选择已发布版本，下载 `.tar.gz` 和对应 `.sha256`。仓库有发行 workflow 不代表已有公开 Release；若列表为空，维护者须完成文末的首次发行步骤。

以下以 `0.3.1` 为例，版本与文件名以实际 Release 为准：

```bash
SHIPBRAID_VERSION=0.3.1
SHIPBRAID_ARCHIVE="shipbraid-${SHIPBRAID_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
curl --fail --location --remote-name \
  "https://github.com/aak1247/shipbraid/releases/download/v${SHIPBRAID_VERSION}/${SHIPBRAID_ARCHIVE}"
curl --fail --location --remote-name \
  "https://github.com/aak1247/shipbraid/releases/download/v${SHIPBRAID_VERSION}/${SHIPBRAID_ARCHIVE}.sha256"
sha256sum --check "${SHIPBRAID_ARCHIVE}.sha256"
tar -xzf "$SHIPBRAID_ARCHIVE"
cd "shipbraid-${SHIPBRAID_VERSION}-x86_64-unknown-linux-gnu"
export SHIPBRAID_BUNDLE="$PWD"
export PATH="$SHIPBRAID_BUNDLE/bin:$PATH"
shipbraid --version
```

校验和验证文件一致性，发行来源仍应通过受信 GitHub Release 确认。解压后的清单覆盖二进制、静态资源和工具；安装器会再次校验全部内容，不接受符号链接、缺失/额外文件或内容改变。不要在安装前修改发行包。自己的项目脚本和秘密放在包外。

## 只安装客户端

已有团队服务时，只需将发行包的 `bin/shipbraid` 放到 PATH，并取得管理员提供的地址和个人 token 文件。无需安装本机协调器或执行器：

```bash
export SHIPBRAID_URL="https://shipbraid.example.com"
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/my-project.token"
shipbraid --json system
shipbraid --json list lane
```

日常操作见 [Agent 指南](agents.md)。不要为已经由其他协调器管理的环境启动第二个实例。

## 本地模拟

此流程只使用发行二进制、Git 和 Python，生成临时模拟源仓库，不发布真实应用；不需要 systemd 或项目部署脚本。

```bash
SHIPBRAID_DEMO_DIR=$(mktemp -d)
mkdir -p "$SHIPBRAID_DEMO_DIR/data"
cp "$SHIPBRAID_BUNDLE/examples/standalone/simulation.toml" "$SHIPBRAID_DEMO_DIR/config.toml"
ln -s "$SHIPBRAID_BUNDLE/web" "$SHIPBRAID_DEMO_DIR/web"
git init --quiet --initial-branch=main "$SHIPBRAID_DEMO_DIR/source"
git -C "$SHIPBRAID_DEMO_DIR/source" -c user.name=Demo -c user.email=demo@example.invalid \
  commit --quiet --allow-empty -m "Synthetic demo source"
git clone --quiet --mirror "$SHIPBRAID_DEMO_DIR/source" "$SHIPBRAID_DEMO_DIR/data/source.git"
export SHIPBRAID_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
shipbraid serve --config "$SHIPBRAID_DEMO_DIR/config.toml"
```

在另一个终端设置相同的 `SHIPBRAID_DEMO_DIR`、PATH 和 `SHIPBRAID_TOKEN`；不要把 token 值提交到仓库或发送给他人：

```bash
REVISION=$(git --git-dir="$SHIPBRAID_DEMO_DIR/data/source.git" rev-parse refs/heads/main)
shipbraid submit --lane demo-test --revision "$REVISION" --components app \
  --idempotency-key first-simulation
shipbraid wait REQUEST_ID --timeout-seconds 120
```

`REQUEST_ID` 替换为 submit 返回的 id。浏览器打开 `http://127.0.0.1:8787`，输入同一 token，即可查看内置 Dashboard；模拟 receipt 不代表真实环境已验收。

## 首次接入自己的项目

项目已有的 `deploy.sh` 负责构建并发布明确 SHA；新增 `verify.py` 每次独立观察实际环境。将它们及依赖放入专门审查的目录，例如 `/srv/myapp/shipbraid-scripts-v1/`。不要把整个项目仓库、`.git`、本地缓存或秘密文件当脚本包。脚本须通过自身路径引用随附资源，工作目录是可写的 `var/work`。

安装器将这个目录复制到固定版本路径；它不会从项目提交中自动选取部署命令。信封、验证脚本输出、环境文件格式和停止证明规则见[本机适配器说明](../integration/local/README.md)。其中示例只实现临时合成环境，真实项目必须提供自己的产物/运行版本观察和健康检查。

先由管理员按既有流程准备受信 bare 镜像和持续同步入口，例如：

```bash
git clone --mirror <受信项目仓库地址> /srv/git/myapp.git
# 后续在提交请求前通过项目维护流程快进同步，不自动强制覆盖分叉历史。
git --git-dir=/srv/git/myapp.git fetch --no-tags origin refs/heads/main:refs/heads/main
```

安装到一个全新的私有目录，以下路径全部替换为项目实际值：

```bash
python3 "$SHIPBRAID_BUNDLE/install.py" \
  --prefix "$HOME/.local/share/shipbraid" \
  --project myapp --environment dev --components app \
  --repository /srv/git/myapp.git --release-ref refs/heads/main \
  --project-bundle /srv/myapp/shipbraid-scripts-v1
```

安装器只支持初始化 dev/test，拒绝覆盖已有 prefix，不启动服务、不提交发布、不更新镜像。它输出配置和 token **文件位置**；token 值保存在 mode0600 文件内。安装前应确认该环境没有其他部署写入所有者。

生成内容：

| 路径（相对 prefix） | 用途 |
| --- | --- |
| `bin/shipbraid` | 指向固定 release 的 CLI |
| `releases/<version>-<digest>/` | 二进制、Dashboard、本机适配器、项目脚本和 adapter.json |
| `etc/config.toml` | 仓库、lane、权限与固定 hooks 路径 |
| `etc/operator.token` / `etc/admin.token` | 分开的普通发布/管理员身份 |
| `etc/tokens.env` | 协调器身份环境文件，不能传给部署脚本 |
| `etc/deploy.env` | 部署/验收专用凭据，KEY=value，无 shell 展开 |
| `var/` | SQLite 账本、任务身份、私有日志与工作目录 |
| `shipbraid.service` | 待检查的 systemd user unit，尚未安装或启动 |

检查配置和复制后的项目脚本，将部署专用凭据写入 `etc/deploy.env`。不要用该文件保存协调器 admin/operator token。保持旧 release 脚本和 adapter.json 不变；变更脚本包时安排维护并生成新版本目录。

服务与 hook 适配器属于管理员审查的受信程序，运行在同一账号权限下。适配器为项目部署/验收脚本重新构造环境，避免把协调器 token 继续传给这些子进程；这不构成针对恶意项目代码的主机隔离。脚本包中隐藏文件、私钥、数据库和日志会被安装器拒绝复制，凭据单独留在 `etc/deploy.env`。

前台启动：

```bash
SHIPBRAID_PREFIX="$HOME/.local/share/shipbraid"
export PATH="$SHIPBRAID_PREFIX/bin:$PATH"
set -a
. "$SHIPBRAID_PREFIX/etc/tokens.env"
set +a
shipbraid serve --config "$SHIPBRAID_PREFIX/etc/config.toml"
```

或在检查现有 unit 没有冲突后，安装为当前用户的常驻服务。下面的存在性检查失败时应停止并按升级流程处理，不能覆盖旧 unit：

```bash
mkdir -p "$HOME/.config/systemd/user"
test ! -e "$HOME/.config/systemd/user/shipbraid.service" && \
  install -m 600 "$SHIPBRAID_PREFIX/shipbraid.service" "$HOME/.config/systemd/user/shipbraid.service"
# 仅在上一步成功、并确认没有旧协调器管理同一环境后执行。
systemctl --user daemon-reload
systemctl --user enable --now shipbraid.service
```

长期运行的 user manager 是否需要 linger，由主机管理员按既有运维约定配置。远程访问默认使用 SSH 转发或受控 HTTPS 代理；安装器不自动开放网络端口。

在另一个终端执行日常发布；完整 SHA 必须已同步到配置镜像：

```bash
export SHIPBRAID_URL="http://127.0.0.1:8787"
export SHIPBRAID_TOKEN_FILE="$HOME/.local/share/shipbraid/etc/operator.token"
shipbraid --json system
shipbraid --json list lane
shipbraid submit --project myapp --environment dev --revision <完整SHA> \
  --components app --idempotency-key <本次意图的稳定键>
shipbraid wait REQUEST_ID --timeout-seconds 600
shipbraid inspect request REQUEST_ID
```

`wait` 超时/断线不会撤销请求。任务启动成功或脚本退出 0 只会进入后续观察/验收；只有匹配环境 receipt 才能 `satisfied`。先在授权环境验证重复交接、重启恢复、错误版本拒绝和实际产物，再把配置位置与命令写进项目 `AGENTS.md`。协议夹具或合成示例通过不能替代项目验收。

## 脚本与两种 webhook

**本机脚本：** 上面的适配器代管协议、持久任务和 systemd；用户只提供部署命令和项目验收脚本。部署任务按不可抢占处理，不接受通过 timeout/kill 推断停止。日志保留在私有任务目录，不原样发送给 API。

**Shipbraid → HTTP 执行器：** 使用现有 hooks 的 `kind = "webhook"`，分别配置 dispatch/observe/verify URL 和 `token_env`。远端须实现相同 JSON 协议；普通通知 webhook 和 GitHub Actions 的原始 workflow_dispatch 不能直接代替完整执行器。可在执行主机运行本机适配器的可选 `/hook` 服务并通过 HTTPS 代理访问，见[适配器说明](../integration/local/README.md)。

**GitHub → Shipbraid：** 运行可选接收器，签名验证通过后将受信仓库和分支映射为 normal/exact 发布请求。配置、HTTPS 代理、凭据和重投见[GitHub 接入说明](../integration/github/README.md)。执行器与事件入口是两个独立配置，不要把 GitHub payload URL 填进 dispatch hook。生产默认不提供映射。

```bash
python3 "$SHIPBRAID_BUNDLE/adapters/github/receiver.py" --config /private/path/github.json
```

接收器不会自动 fetch Git 镜像。先同步镜像再提交/重投；镜像缺 SHA、上游拒绝或超时返回非成功，按 GitHub 交付记录重投同一 delivery，保持幂等键。未接公网 GitHub 的情况下，只能声明本地签名事件和 API 已验收。

## 升级与恢复

新安装器不负责存量升级，也不能用新空目录替换有活动执行的实例。升级需按已有维护流程：

1. 用管理员身份 `shipbraid drain`，等待并核对所有活动 run 已结算；unknown 继续保留所有权，不靠超时或杀进程解锁。
2. 用 SQLite 在线备份 API 做一致备份，保存配置、token 引用、镜像、任务目录、当前版本及所有冻结 hook 路径。不要只复制活动中的单个 `.db` 文件。
3. 停止协调器，解压并校验新发行包到**新的版本目录**。保持同一个数据库、lane 身份和旧执行器路径。停止协调器不会停止独立任务。
4. 更新服务使用的新二进制和 Dashboard 路径，旧 release 和项目适配器继续保留。变更适配器/profile 时单独验证，不在旧目录原地修改。
5. 启动后检查 `system`、lane、未结请求及所有权、环境和鉴权，再 `shipbraid resume`。

数据库发生迁移后不能保证旧二进制可读；回退前检查 schema 兼容与备份方案。恢复或替换数据前仍须核对独立执行器，防止回退账本丢失实际部署。旧名称安装另见[升级说明](naming.md)。

## 维护者：源码构建和首次发行

下面是维护者工作流，普通使用者无需执行。先通过 Rust/前端检查，构建后生成发行包：

```bash
cargo build --release --locked
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
python3 scripts/package-release.py
python3 scripts/acceptance-distribution.py \
  --bundle target/dist/shipbraid-0.3.1-x86_64-unknown-linux-gnu.tar.gz
```

本地包使用本机 ABI；本地验收不等同于官方 Ubuntu 22.04 runner 验收。工作树须完成审查和提交后，再对与 Cargo 版本一致的提交创建 `v<version>` tag，并按项目发布权限推送。Release workflow 会重新检查、构建、验证包并生成 **draft Release**。维护者检查工件后手动发布；不会把未提交修改作为公开 tag 的发行来源。

发行包验收只操作临时合成环境与独立测试 unit，不会发布业务应用或修改集群权限。已执行的本轮证据见[发行执行计划](features/iter-20261017-distribution/execution-plan.md)。
