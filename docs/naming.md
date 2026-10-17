# Shipbraid 命名与现有安装升级

项目标识统一为 `shipbraid`。品牌标题可写作 Shipbraid；命令、包名、环境变量和安装路径使用下表。

| 对象 | 当前标识 |
| --- | --- |
| Cargo package / Rust library / CLI | `shipbraid` |
| Dashboard package | `shipbraid-dashboard` |
| CLI 连接与身份 | `SHIPBRAID_URL`、`SHIPBRAID_TOKEN`、`SHIPBRAID_TOKEN_FILE` |
| 示例执行器令牌 | `SHIPBRAID_EXECUTOR_TOKEN` |
| 安装器操作者 / 管理员环境变量 | `SHIPBRAID_OPERATOR_TOKEN`、`SHIPBRAID_ADMIN_TOKEN` |
| 用户服务 | `shipbraid.service` |
| 新配置与数据目录 | `~/.config/shipbraid/`、`~/.local/share/shipbraid/` |
| 本机适配器任务 | `shipbraid-local-<run_uuid_hex>-g<generation>.service` |

新 CLI 不读取旧 `AUTO_DELIVER_*` 变量。使用自己的 token 文件，例如：

```bash
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/operator.token"
shipbraid --json system
shipbraid --json list lane
```

配置中的 `token_env` 仍是管理员可配置的环境引用；发行包示例和安装器统一生成 `SHIPBRAID_*`。协议、lane ID、请求/run/receipt ID、generation、tenant 和数据库表结构不因命名改变。

## 现有安装

发行包安装器只创建新的私有 prefix，不会迁移已有安装，也拒绝覆盖已存在的 prefix。它不能用来替换正在管理部署边界的协调器或账本。

已有安装升级由操作者安排维护：

1. 使用当前 CLI 和凭据 drain，等待并核对活动执行。`unknown` 或停止尚未证实时继续保留原所有权；不要靠超时、kill 或删库解锁。
2. 用 SQLite 在线备份 API 取得一致备份，记录配置、受信镜像、token 引用、冻结执行器路径和未结请求/执行身份。任务运行目录不能丢弃。
3. 保留旧运行的 `executor_ref` 及冻结 hook 配置。旧执行仍由创建它的受信执行器观察；新适配器不能据此继承或释放旧所有权。
4. 在维护范围内准备新版本的 unit、CLI 和环境变量引用，并继续使用同一个账本、镜像与任务状态。不要让新旧服务用不同数据库同时管理相同部署边界。
5. 确认执行已结算后停止旧协调器，安装新二进制并使用已核对的配置启动。停止协调器本身不证明独立执行器已停止。
6. 启动后核对请求/run/receipt、lane 所有权、环境、鉴权与观察，再按运维流程恢复调度。若 schema 或冻结 hook 配置变化，先单独验证兼容和恢复过程。

## 历史记录与仓库托管地址

`docs/acceptance/` 和已完成的历史 feature 中保留当时实际使用的服务名、二进制路径、CI 地址、命令和接入项目细节，不将验收记录改写为当前用法。当前安装与接入入口见 [发行包指南](distribution.md)、[Agent 指南](agents.md)和 [hooks 指南](hooks.md)。

2026-10-17，GitHub 仓库已改名为 [`aak1247/shipbraid`](https://github.com/aak1247/shipbraid)，本地 `origin` 和 README clone 地址统一为 `https://github.com/aak1247/shipbraid.git`。改名前后仓库 ID 均为 `1421990668`，默认分支仍为 `main`；旧 API 地址也解析到该相同仓库。此操作未移动本机 checkout 目录，历史 CI 链接与验收记录保留原地址。

## 历史改名验收

仓库改名时记录的 Rust fmt/clippy/test/build、前端 typecheck/test/build，以及模拟、hooks、多 Agent、CLI、Bridge 与安装器预检结果见[改名验收](acceptance/shipbraid-rename-20261017.md)。这些检查只使用本地模拟或隔离夹具，不触发真实发布；Bridge 验收属于当时的项目集成记录，不代表当前 Shipbraid 仓库内置该项目适配器。
