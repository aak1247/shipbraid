# 接入自己的项目

Shipbraid 协调发布请求，项目仍提供自己的构建、部署和独立验收。源码 0.3.2-dev 的单项目默认使用[本机前台 CLI](local-cli.md)，不需要 HTTP 服务或客户端 token；跨项目、多个提交者并发或 Dashboard 使用 Server。已发布的 v0.3.1 仅支持原 Server/client 流程，尚无 local/run 功能。

项目已有团队服务时，开发者安装兼容 CLI，取得地址、个人 token 文件和已注册 lane。不要为已经由服务管理的环境再启动一个独立账本。新装、接入和升级按[发行包指南](distribution.md)操作。

```text
项目 Agent → shipbraid CLI → 已登记 lane → 项目执行器 → 现有构建/部署工具
                                      ↑                    ↓
                                  请求状态 ← 独立环境验收与 receipt
```

| 角色 | 工作内容 |
| --- | --- |
| 项目接入者 | 明确环境、组件、发布线和验收证据；选择或实现执行器，记录接入结果 |
| 服务管理员 | 配置持久服务、受信镜像、lane、权限、执行器和凭据；维护镜像同步 |
| 日常开发 Agent | 按项目约定提交代码、确认版本同步、提交请求并核对最终 receipt |

日常 Agent 通过 PATH 直接调用发行包中与团队服务兼容的 `shipbraid` 二进制。

## 接入前收集信息

在项目部署文档中确定下表内容，秘密值保存在仓库外。dev、test、prod 应分别登记 lane、访问权限和适用参数。

| 信息 | 示例或用途 |
| --- | --- |
| 运行入口 | local 的私有配置/账本位置；或 server 的 URL、Agent principal、私有 token 文件位置 |
| 项目目标 | project、environment、lane 和允许发布的组件 |
| 发布线 | 受信仓库身份、服务端本地镜像、允许的 ref |
| 同步方式 | Agent 如何推送，既有流程如何更新并确认镜像 |
| 执行入口 | 能接收明确 revision 并调用现有构建/发布工具的受控脚本、CI 或适配器 |
| 状态与验收 | 如何独立观察执行状态、实际版本、各组件摘要、配置指纹和环境健康 |
| 恢复流程 | unknown、部分发布失败和失败暂停的核对人及处置流程 |

调用方只能选择管理员登记的目标和组件，不能从请求中提供仓库 URL、部署命令、路径或凭据。版本必须是完整 commit，已进入受信镜像并符合发布线；CLI 不会替项目推送或同步版本。

## 管理员准备项目

管理员先登记项目名、环境、组件、受信 Git 镜像、发布线、lane 权限和配置指纹。镜像应由受信同步流程更新；提交前确认完整 SHA 已存在并符合发布线。调用者只能选择已登记的 lane 和组件，不能在请求中指定仓库地址、部署命令或凭据。

为每个环境选用经审查的执行器：本地脚本由固定 command/script hooks 或[随发行包提供的本机适配器](../integration/local/README.md)执行；远端 HTTP 执行器通过 webhook 实现相同的 `dispatch`、`observe`、`verify` 协议。普通通知 webhook 不等于执行器。程序、参数、URL、工作目录和凭据引用均由管理员配置。执行器需支持重复/不确定交付的安全处理；协调器按至少一次命令/事件交付设计。

适配器应使用稳定身份关联 run 与外部任务，并能在协调器重启后继续观察原任务。`dispatch` 快速交接，`observe` 只读观察，`verify` 独立验收；完整信封、停止证明和失败处理规则见[外部执行器协议](hooks.md)。项目自定义请求参数由 lane 的 `request_parameters` 声明，参数会冻结到发布目标并进入幂等内容；不得把秘密定义为请求参数，具体限制见[项目请求参数](request-parameters.md)。

对于新服务，管理员按发行包指南安装并维护单个协调器实例；接入已有服务时把 lane、身份和执行器配置加入该服务管理流程。不要为同一环境另起独立账本，也不要更改或覆盖正在被活动 run 引用的冻结配置。

## 在真实环境验收

先在明确授权的非生产 lane 验证项目自己的脚本与观察器。用已同步的固定 SHA 发起一次请求，并检查：

- 重复 dispatch 或响应丢失不会重复启动部署；无法确认状态时保留 `unknown` 和 lane 所有权。
- 部署完成、脚本返回成功、CI 绿色都不会单独满足请求；verify 会独立检查实际版本、每个请求组件的摘要、配置指纹和健康状态。
- 失败停止证明来自当前 run 的稳定执行身份和新鲜的物理停止证据。超时、心跳丢失或停止协调器都不算停止证明。
- 协调器重启后继续观察原执行。只有匹配的环境 receipt 才能将请求标为 `satisfied`。

模拟执行器和合成项目只验证 Shipbraid 协议，不证明真实环境已接通。生产 lane 需另行明确授权并完成同等级的环境验收；不得因测试 lane 通过而自动开放生产权限。

## 项目发布指引（权威知识来源）

从 0.3.2-dev 开始，lane 配置支持可选的三级自然语言发布指引 `[lanes.guide]`，作为项目发布知识的**权威来源**：

```toml
[[lanes]]
id = "myapp-dev"
# ...其他字段不变...
[lanes.guide]
project = "guides/myapp/overview.md"        # 项目总述：发布线、镜像同步、审批边界
environment = "guides/myapp/dev.md"         # 环境流程、参数要求、注意事项
[lanes.guide.components]
app = "guides/myapp/app.md"                 # 组件单独说明，键必须是 lane.components 成员
worker = "guides/myapp/worker.md"
```

- **统一维护**：指引由管理员在协调器配置中统一维护，与 lane 范围绑定，告别各个业务仓库手抄模板的陈旧和漂移。
- **动态查询**：Agent 可通过 API（`GET /v1/lanes/{id}/guide` 与 `GET /v1/guides/{project}`）或 MCP 工具按需获取最新指引，响应携带内容 SHA-256 供调用方审计。
- **安全检查**：指引内容在协调器启动时一次性读入快照，实行 fail-closed 秘密扫描（严禁私钥、云凭据与平台 Token）与单文件 32 KiB 上限；doctor 工具提供只读合规检查。
- **模板兜底**：下方的项目 `AGENTS.md` 模板继续保留，用于项目尚未配置在线指引时的初始参考与本地兜底。

## 项目 `AGENTS.md` 参考模板

选择与实际安装一致的入口，记录 binary 兼容版本、受信镜像同步和独立验收证据。配置、账本、任务目录和凭据保存在源码仓库外。

### 单项目 local 模板（0.3.2-dev+）

````markdown
## 使用 Shipbraid 发布

- 入口：local；配置：<私有绝对路径>；binary：<包含 run 功能的发行版本>。
- 目标：<项目、环境、lane、允许组件>；版本同步：<受信镜像同步与确认方式>。
- 本配置只管理本项目，账本同时只能由一个 run 持有；不启动另一个账本管理相同部署环境。
- 不运行 HTTP 客户端命令来查询本地账本；按原 request id 恢复前台调度。
- CLI 超时/中断不撤回请求，也不证明独立部署已停止；Unknown 保留所有权，不盲目换 key 或强制解锁。
- 只在请求 satisfied 且 receipt 匹配本次目标时报告成功；生产操作仍需项目的明确授权。
- 不提交数据库、任务目录、私钥、token、部署凭据或秘密日志。

```bash
shipbraid --json run --config <私有配置路径> --lane <已登记lane> \
  --revision "$(git rev-parse HEAD)" --components app --task-id issue-42
# 已有请求：恢复原请求，不能用新提交代替状态查询。
shipbraid --json run --config <同一配置路径> --resume REQUEST_ID
```

SHA 必须已同步到配置镜像。仅传 lane 已声明的非秘密参数。保存 request id 与幂等键；失败重试只在独立停止证明成立后显式使用 --resume REQUEST_ID --retry。
````

### 共享 Server 模板

把以下约束和实际 lane、组件、token 文件位置合并到项目自己的 Agent 指南。token 只放在受控文件或凭据系统中，不提交其值。

````markdown
## 使用 Shipbraid 发布

- 接入状态：<已验收环境与验收记录；未验收时明确标注待验收>。
- 服务：<团队 URL>；CLI：<发行包来源与兼容版本>。
- 身份：<principal>；token 文件：<私有绝对路径>。
- 目标：project=<项目>；environment=<环境>；lane=<通道>；组件：<组件名>。
- 版本同步：<推送目标、镜像同步入口及如何确认完成>。
- 参数：<lane 声明的必填参数和允许值；无则写无>。
- 授权：<允许发布的环境与动作；生产操作边界>。
- 故障处理：<unknown / needs_attention 核对步骤或维护者>。
- 执行器维护：<配置位置、适配器源码和升级流程>。
- 只有请求为 `satisfied` 且 receipt 匹配本次 revision、组件和环境配置时，才报告发布成功。
- 本地等待超时、连接中断或 `unknown` 不表示请求撤回或执行停止；用原 request id/key 恢复，不要盲目换 key、手动覆盖环境或释放 lane。
- 不提交 token、私钥、完整数据库、部署凭据或含秘密的日志。

从已同步的项目提交运行，并把示例值替换为本项目登记值：

```bash
export SHIPBRAID_URL="https://shipbraid.example.com"
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/project-agent.token"
shipbraid --json system
shipbraid --json inspect lane demo-test
shipbraid --json list request --task-id issue-42
shipbraid --json submit --lane demo-test --revision "$(git rev-parse HEAD)" \
  --components app --task-id issue-42 --param change_window=scheduled
shipbraid --json wait REQUEST_ID --timeout-seconds 300
```

只对该 lane 已声明的参数传入 `--param`；不需要请求参数时删去该选项。保存 request id 和返回的幂等键。日志命令按部署 CLI 版本选择：已发布 v0.3.1 使用 `shipbraid logs RUN_ID`；当前源码 `0.3.2-dev` 还提供 `logs run` 与 `logs request`，需等后续发行。
````

CLI 用法、版本与恢复边界见[Agent 指南](agents.md)和[HTTP API/CLI 契约](api.md)。
