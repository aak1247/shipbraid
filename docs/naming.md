# Shipbraid 命名与旧安装升级

项目标识现统一为 `shipbraid`。品牌标题可写作 Shipbraid；命令、包名、环境变量和安装路径使用下表。

| 对象 | 当前标识 |
| --- | --- |
| Cargo package / Rust library / CLI | `shipbraid` |
| Dashboard package | `shipbraid-dashboard` |
| CLI 连接与身份 | `SHIPBRAID_URL`、`SHIPBRAID_TOKEN`、`SHIPBRAID_TOKEN_FILE` |
| 示例执行器令牌 | `SHIPBRAID_EXECUTOR_TOKEN` |
| 安装器操作者 / 管理员环境变量 | `SHIPBRAID_OPERATOR_TOKEN`、`SHIPBRAID_ADMIN_TOKEN` |
| 用户服务 / 局域网转发 | `shipbraid.service`、`shipbraid-lan.socket`、`shipbraid-lan.service` |
| 新配置与数据目录 | `~/.config/shipbraid/`、`~/.local/share/shipbraid/` |
| 新执行器任务 | `shipbraid-autotask-<run_id>-<generation>.service` |

新 CLI 不读取旧 `AUTO_DELIVER_*` 变量。使用自己的 token 文件，例如：

```bash
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/operator.token"
shipbraid --json system
shipbraid --json list lane
```

配置中的 `token_env` 仍是管理员可配置的环境引用；示例和安装器统一生成 `SHIPBRAID_*`。协议、lane ID、请求/run/receipt ID、generation、tenant 和数据库表结构不因改名改变。

## 现有安装

改名只修改仓库和新安装默认值，不自动改动正在运行的服务、真实数据库、执行器或凭据。安装器同时检查新旧服务；检测到活动协调器、无法确认服务状态，或旧配置/数据/unit 文件时，会拒绝自动安装，避免另起一个空账本协调器。

已有安装需由操作者安排维护升级：

1. 使用旧 CLI/凭据 drain，等待并核对全部活动执行。unknown 或停止尚未证实时继续保留原所有权；不要靠超时、kill 或删库解锁。
2. 用 SQLite 在线备份 API 取得一致备份，记录配置、受信镜像、token 引用、冻结执行器路径和未结请求/执行身份。任务运行目录不能丢弃。
3. 确认执行已结算后停用旧协调器与旧 LAN 入口。停止协调器本身不证明独立执行器已停止。
4. 在维护范围内准备新命名的 unit、CLI 和环境变量引用，并保留同一个账本、镜像与任务状态；也可继续引用其原绝对数据路径。不要让新旧服务用不同数据库同时管理相同部署边界。
5. 保留旧运行的 `executor_ref` 及冻结 hook 配置，不能把历史 unit 字符串直接替换成新前缀。Bridge 能识别相同 run/generation 的旧任务身份，并继续只读观察；这不表示可以绕过执行器配置指纹检查。
6. 新服务启动后核对请求/run/receipt、lane 所有权、禁用生产状态、鉴权与观察，再按运维流程恢复调度。若需移动配置/数据或替换 hook 路径，需单独验证冻结配置兼容和恢复过程，不能把新安装器当自动迁移工具。

本轮没有执行上述维护动作，也没有宣称已有本机服务已经改名。[原只读审计](acceptance/production-readiness-20261017.json)记录了当时的恢复阻塞与生产准入问题。随后独立的[失败恢复修复](acceptance/hook-failure-recovery-20261017.json)保留旧 unit/数据路径完成协调器维护，并凭新鲜停止证据将旧 unknown 执行结算为失败；生产仍禁用。

## 历史记录与仓库托管地址

`docs/acceptance/` 和已完成 feature 中的验收记录保留当时真实的 `auto-deliver` unit、二进制路径、GitHub CI URL 和命令，避免将历史证据改写成未执行过的新名称。这些记录不作为当前命令用法；当前入口见 README、[Agent 指南](agents.md)和[hooks 指南](hooks.md)。

当前配置的 Git 远端仍是 `https://github.com/aak1247/auto-deliver.git`，因此 README 的 clone 地址保留真实托管地址，checkout 目录为 `shipbraid`。远端仓库本身及本地 checkout 目录未被此代码改名操作移动。

## 本轮验证

Rust fmt/clippy/test/build、前端 typecheck/test/build，以及模拟、hooks、多 Agent、CLI、Bridge 与安装器预检测试的结果见[改名验收](acceptance/shipbraid-rename-20261017.md)。这些检查仅使用本地模拟或隔离夹具，不触发真实发布。
