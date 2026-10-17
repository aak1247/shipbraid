# Agent 发布管理

Shipbraid 接受同一团队内多个 Agent 通过 HTTP 或 CLI 协作。当前租户由服务端固定为 `local`，不接受客户端指定。项目与 CLI 统一使用 `shipbraid`，环境变量使用 `SHIPBRAID_` 前缀。项目接入与项目级 `AGENTS.md` 模板见[项目接入指南](project-integration.md)；已有安装升级见[改名说明](naming.md)。

## 先确认项目已经接入

Shipbraid 协调发布请求，构建、部署和环境验收由项目配置的执行器完成。安装 CLI 或配置 token 不会自动接入当前仓库。先读当前项目的 `AGENTS.md` 或部署文档，找到团队服务地址、个人 token 文件、目标 lane、组件、允许的发布线和镜像同步方式。安装发行包中的 `shipbraid` 二进制后直接调用；日常 Agent 不需要源码构建。

```bash
export SHIPBRAID_URL="https://shipbraid.example.com"
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/my-project-agent.token"
# 若已设置 SHIPBRAID_TOKEN，先 unset，避免 token 来源冲突。
shipbraid --json system
shipbraid --json list lane
shipbraid --json inspect lane LANE_ID
```

确认 lane 的项目、环境、组件、启用状态、执行器类型和阻塞原因。`scripted` 用于模拟；真实发布只走项目已经验收并明确授权的 lane。通道可见不代表获得了本次生产发布授权。

| 情况 | 下一步 |
| --- | --- |
| lane 已启用、可用且身份匹配 | 按项目约定提交并跟踪请求 |
| 找不到服务、凭据或目标 lane | 向项目维护者取得接入信息；授权列表为空不能证明服务端没有配置 lane |
| 项目尚未接入或只有模拟 lane | 由维护者按[项目接入指南](project-integration.md)配置并验收执行器 |
| lane 被禁用、失败暂停或处于 `unknown` | 按项目运维流程核对；停止未经证明时保留执行所有权 |

## 已接入项目的发布步骤

提交前确认代码已按项目约定推送，并由受信同步流程写入服务端镜像；本地 commit 本身不足以让协调器接受该版本，CLI 不会推送代码或更新镜像。核对目标环境、组件和声明参数。已有任务先按 task id 查找；只接手跟踪时等待原请求，不要另建一个身份请求。

```bash
REVISION=$(git rev-parse HEAD)
shipbraid --json submit \
  --lane my-project-dev --revision "$REVISION" --components app \
  --task-id issue-42 --param change_window=scheduled
# 将 REQUEST_ID 替换为提交结果的 id。
shipbraid --json wait REQUEST_ID --timeout-seconds 300
shipbraid --json inspect request REQUEST_ID
```

示例中的 lane、组件、任务和参数必须替换为项目实际值；只有 lane 声明的参数可提交。保存 request id 与幂等键。只有请求为 `satisfied` 且 receipt 匹配本次版本、组件和环境配置时，才能报告发布成功；本地等待超时继续查询原请求。

## 配置身份

由操作者为每个稳定身份配置独立 token。例如在配置中为已有 `demo-test` 通道追加：

```toml
[[tokens]]
id = "agent-a"
token_env = "SHIPBRAID_AGENT_A_TOKEN"
lanes = ["demo-test"]

[[tokens]]
id = "agent-b"
token_env = "SHIPBRAID_AGENT_B_TOKEN"
lanes = ["demo-test"]
```

服务启动前在私有环境中设置这些变量；不要将值写入配置或仓库。每个 Agent 用自己的 token 文件访问：

```bash
export SHIPBRAID_TOKEN_FILE="$HOME/.config/shipbraid/agent-a.token"
# 若此前设置过 SHIPBRAID_TOKEN，应先 unset，避免两种凭据来源冲突。
shipbraid --json system
shipbraid --json list lane
```

`system` 返回 API 版本、`tenant_id`、认证主体与主体能力。通道列出允许的组件、覆盖模式、是否启用、执行器类型和抢占能力；不会暴露镜像路径或凭据。`urgent` 与 `admin` 独立配置，普通 Agent 不需要管理员权限。

## 提交并等待

```bash
REVISION=$(git rev-parse HEAD)
shipbraid --json submit \
  --project demo --environment test \
  --revision "$REVISION" --components app \
  --task-id issue-42 --session-id session-a1 \
  --collaborator agent-b

# 将 REQUEST_ID 替换为提交结果的 id。
shipbraid --json wait REQUEST_ID --timeout-seconds 60
```

版本必须已存在于管理员配置的受信镜像并满足发布线规则，CLI 不会根据客户端地址自行拉取仓库。默认 exact；descendant 需要通道显式启用，且 Git 祖先关系不证明功能没有被 revert。

提交结果包含 `id`、`idempotency_key`、`reused`、当前 run、请求状态和 `allowed_actions`。请求、run 与 receipt 是不同实体；最终成功以请求 `satisfied` 和匹配 receipt 为准。

- `task_id`：开发任务关联，多个请求可共享同一任务标识。
- `session_id`：首次创建请求的会话追踪；不授予权限，不影响幂等，重放不会覆盖首次值。
- `collaborators`：有权管理该请求的已配置主体，提交时冻结，最多 16 个；必须已能访问对应通道，不能填 executor 身份。

## 网络不明与会话恢复

带 `--task-id` 且未显式指定幂等键时，CLI 根据任务和规范化提交意图生成确定键。组件、协作者排序去重，会话不参与。CLI 将目标选择解析为同一个 lane，并将截止时间规范为 UTC；保持相同提交意图，同一个 principal 可以跨会话安全重放；改变版本、目标、截止时间、优先级、原因、协作者或请求参数，会生成新键。显式 `--idempotency-key` 覆盖默认策略。

没有 task 或显式 key 时仍会生成随机键。CLI 在提交成功和提交 HTTP/传输错误的 JSON 中返回 `idempotency_key`，应保留它。遇响应丢失，不可根据网络错误推断请求未受理；使用原键与原正文恢复，不生成新键盲重试。

新 Agent 或丢失本地会话时，先按任务找回：

```bash
shipbraid --json list request --task-id issue-42 --limit 20
shipbraid --json inspect request REQUEST_ID
shipbraid --json wait REQUEST_ID --timeout-seconds 60
```

不同 principal 的同键提交仍是独立请求。后继 Agent 若只接手跟踪，应查询并等待原请求；需要自己独立的上线需求时再提交。兼容请求会共享执行，但不会合并身份、截止时间与最终结果。

列表可按 `--principal agent-a`、`--status waiting`、`--lane demo-test` 过滤。请求列表默认 100 条，最多 200；当 `next_cursor` 非 null 时，用同一组过滤条件传入 `--cursor` 继续。游标不是授权凭证，也不保证跨页状态快照；恢复任务应从首页重新查询。

## 协作权限与状态解释

同一通道的授权 Agent 可以查看请求。撤回、重试和修改优先级要求通道权限，并且是请求本人、冻结的协作者或管理员。提交/提升 urgent，以及 retry 已有 urgent 请求，都另需 urgent 或 admin；撤回 urgent 不需要该额外能力。

```bash
shipbraid --json withdraw REQUEST_ID
shipbraid --json retry REQUEST_ID
shipbraid --json priority REQUEST_ID urgent --reason "incident fix"
```

先读取 `allowed_actions`（`withdraw`、`retry`、`raise_priority`），结合 `waiting_reason`、`blocked_reason`、receipt 判断下一步。提示只是响应时快照，实际执行会重新检查权限与状态。`retry` 保留原目标和截止时间，不能用它跳过 unknown 所有权核对；旧执行的停止未经证明时继续等待或由管理员调查。

撤回一个请求不会撤回共享 run 上的其他请求。事件中的 `actor` 是实际操作主体；旧事件/内部系统事件可为 null。

外部 hooks v1 不可抢占，urgent 仅提高排队优先级。协调器不为多个 Agent 合并分叉代码，也不自动执行回滚。

## 机器输出与退出码

成功命令输出 JSON。`--json` 模式下，HTTP、网络、本地配置、参数错误也在 stdout 输出：

```json
{"error":{"code":"forbidden","message":"The configured token is not authorized for this operation.","retryable":false,"http_status":403}}
```

`http_status`、`retry_after_seconds`、`idempotency_key` 是可选字段。参数错误 code 为 `invalid_arguments`，非结构化本地错误为 `local_error`；截止时间格式错误为 `invalid_deadline`；传输错误为 `transport_error`，非 JSON 响应为 `invalid_response`。恢复时检查 code 和 retryable，不解析人类错误文本。help/version 保持普通文本。

`wait` 对可重试的只读查询错误在本地期限内退避并尊重 Retry-After；不会重提发布。超时返回最后观察（若没有观察则 `observation_available=false`），并带 `local_wait_timeout=true`。旧观察只是历史信息，不能证明当前环境。退出码如下：

| 码 | 意义 |
| --- | --- |
| 0 | wait 已满足；其他命令调用成功 |
| 1 | 命令、配置或不可恢复调用错误 |
| 2 | wait 需处理；或参数错误（结合 JSON/请求状态区分） |
| 3 | 请求过期 |
| 4 | 请求已撤回 |
| 124 | 本地等待超时，请求仍在服务器上 |

## CLI 命令参考

本节对应仓库当前源码版本 `0.3.2-dev` 中的 [CLI 实现](../src/main.rs)。尖括号表示必填值，方括号表示可选项；示例中的 lane、身份和资源 id 都要替换成项目实际值。发行包安装后将 `shipbraid` 加入 PATH；日常使用直接调用二进制，不需要 Cargo。

### 全局选项与命令总览

```text
shipbraid [全局选项] <命令> [参数]
shipbraid --help
shipbraid <命令> --help
shipbraid help [命令]
shipbraid --version
```

| 全局选项 | 环境变量 | 默认值与说明 |
| --- | --- | --- |
| `--json` | 无 | 成功响应本来就是 JSON；启用后，参数、配置、HTTP 和网络错误也写成 stdout 上的结构化 JSON。help/version 仍输出文本 |
| `--url <URL>` | `SHIPBRAID_URL` | `http://127.0.0.1:8787`；远程服务必须用 HTTPS，HTTP 只允许 loopback；不接受 URL 用户名/密码、query 或 fragment |
| `--token <TOKEN>` | `SHIPBRAID_TOKEN` | API bearer token；建议用 token 文件，避免令牌进入 shell 历史 |
| `--token-file <PATH>` | `SHIPBRAID_TOKEN_FILE` | 读取文本文件并去掉首尾空白；与命令行或环境中的 `--token` / `SHIPBRAID_TOKEN` 互斥 |
| `-h, --help` | 无 | 显示当前命令帮助，不访问服务端 |
| `-V, --version` | 无 | 显示 CLI 版本，不访问服务端 |

全局选项可放在子命令前后。命令行值优先于环境变量。除 `serve`、`health` 和 help/version 外，API 命令需要 token。通常返回 15 秒后超时；`wait` 另有本地等待期限。

| 命令 | 用途 | 权限边界 |
| --- | --- | --- |
| `serve` | 按配置启动持久协调器 | 服务配置和执行器访问权限 |
| `submit` | 创建不可变发布请求 | 目标 lane；urgent 还需 urgent/admin 能力 |
| `wait` | 轮询一个请求 | 对应 lane 的读取权限 |
| `inspect` / `list` | 查询 request、run、lane | token 可访问的 lane 范围 |
| `system` | 查看身份、能力、版本和 drain 状态 | API 身份 |
| `events` / `logs` | 读取审计事件或执行日志 | 对应 lane 的读取权限 |
| `withdraw` / `retry` / `priority` | 管理请求 | lane 权限且为请求本人、冻结协作者或 admin；urgent 规则另见[权限说明](#协作权限与状态解释) |
| `drain` / `resume` | 暂停或恢复新执行派发 | admin |
| `resolve-unknown` | 依据独立执行器证据核对 unknown | admin 及对应 lane 权限 |
| `health` | 检查 `/ready` | 无需 token |

`system` 中主体的能力不代表可操作所有请求；服务端对每次写操作重新检查 lane、主体和请求状态。

### serve：启动协调器

```text
shipbraid serve --config <PATH>
```

`--config` 必填，读取 TOML 服务配置。命令以前台服务方式持续运行，日志写入 stderr；它不输出一次性成功 JSON。`serve` 供服务管理员部署协调器，项目开发 Agent 使用团队已运行的服务。配置见[hooks 指南](hooks.md)和[发行包指南](distribution.md)。

### submit：提交发布请求

```text
shipbraid submit (--lane <LANE_ID> | --project <PROJECT> --environment <ENV>) \
  --revision <COMMIT> --components <COMPONENTS> [选项]
```

| 参数 | 必填/默认值 | 说明 |
| --- | --- | --- |
| `--lane <LANE_ID>` | 与 project/environment 二选一 | 选择已授权 lane；不可与 `--project` 或 `--environment` 混用 |
| `--project <PROJECT>` | 不传 lane 时必填 | 必须同时传 `--environment`，并解析到一个已授权 lane |
| `--environment <ENV>` | 与 project 配对 | 项目下已登记的目标环境 |
| `--revision <COMMIT>` | 必填 | 完整不可变 commit，须存在于受信镜像并符合发布线约束 |
| `--components <COMPONENTS>` | 必填 | 逗号分隔，如 `app,worker`；CLI 排序并去重 |
| `--coverage <MODE>` | `exact` | `exact` 或 `descendant`；后者需 lane 显式允许 |
| `--priority <PRIORITY>` | `normal` | `normal` 或 `urgent`；urgent 另需 urgent/admin 能力 |
| `--deadline <TIMESTAMP>` | 不设置 | 含时区的 RFC3339 时间；CLI 转为 UTC |
| `--reason <TEXT>` | 空字符串 | 请求原因；服务端限制为 2 KiB |
| `--idempotency-key <KEY>` | 自动生成 | 显式恢复键；未指定时，有 `--task-id` 则按任务和规范化意图生成，否则生成随机 UUID |
| `--task-id <ID>` | 不设置 | 稳定任务关联；最多 256 字节且不得含控制字符 |
| `--session-id <ID>` | 不设置 | 首次创建会话追踪；最多 256 字节且不得含控制字符，不授予权限也不参与幂等 |
| `--collaborator <PRINCIPAL>` | 无 | 可重复；必须是已配置且有 lane 权限的非 executor 身份，最多 16 个；排序去重 |
| `--param <KEY=VALUE>` | 无 | 冻结的非秘密项目参数；最多 16 项，可重复传入但键名不得重复，键和值不能为空。lane 通过 `request_parameters` 声明允许值和必填项，详见[项目请求参数](request-parameters.md) |

这里没有通用 `--context` 参数。提交上下文通过 `--task-id`、`--session-id` 和 `--collaborator` 指定；HTTP 请求也使用这些字段。参数和上下文一同冻结到请求，参数参与幂等内容。不要把 token、凭据或秘密放进 `--param`、task/session 字段或 reason。

```bash
shipbraid --json submit \
  --lane demo-test --revision "$REVISION" --components app \
  --task-id issue-42 --session-id session-a1 \
  --collaborator agent-b --param change_window=scheduled
```

成功响应中含 `idempotency_key`；非 `--json` 模式还会在 stderr 提示该键。HTTP 请求已受理不等于发布完成，应继续查询同一 request。提交细节和恢复策略见[提交并等待](#提交并等待)与[网络不明与会话恢复](#网络不明与会话恢复)。

### wait：等待请求结果

```text
shipbraid wait <REQUEST_ID> [--timeout-seconds <SECONDS>] [--poll-ms <MILLISECONDS>]
```

`--timeout-seconds` 默认为 `300`，只限制客户端等待；`--poll-ms` 默认为 `500` 且必须大于零。零秒仍进行一次有界查询。`wait` 不输出日志或连续状态流：遇到 `satisfied`、`needs_attention`、`expired` 或 `withdrawn` 时输出一次请求 JSON。`needs_attention` 的退出码为 `2`，表示需要人工处理；`expired` 为 `3`，`withdrawn` 为 `4`。本地超时为 `124`，输出最后一次观察或 `observation_available=false`，并设置 `local_wait_timeout=true`。超时、断开或停止本地轮询不会撤回服务端请求。

### inspect 与 list：查询资源

```text
shipbraid inspect <request|run|lane> <ID>
shipbraid list <request|run|lane> [--lane <LANE_ID>] [--limit <COUNT>]
shipbraid list request [--task-id <ID>] [--principal <PRINCIPAL>]
  [--status <STATUS>] [--cursor <CURSOR>]
```

`inspect` 返回单个资源 JSON：request 含状态、原因、receipt 和 `allowed_actions`；run 含阶段、generation、执行器身份和替代关系；lane 含配置摘要与所有权状态。

`list` 的 `--lane` 可用于三种资源。`--task-id`、`--principal`、`--status`、`--cursor` 仅适用于 request。request status 可为 `accepted`、`waiting`、`satisfied`、`needs_attention`、`withdrawn`、`expired`。request 列表默认 100 条，`--limit` 允许 `1..200`，按创建顺序从新到旧分页并返回 `{items,next_cursor}`。保留全部过滤条件并原样传回不透明的 `next_cursor`；它不是权限凭证，也不构成跨页快照。CLI 只提供 `--lane` 过滤，不提供 API 支持的 project/environment 列表过滤。run 和 lane 列表返回 `{items}`；虽然 CLI 接受 `--limit`，服务端目前不会对这两类列表应用它们，也没有 cursor 分页能力。

```bash
shipbraid --json list request --task-id issue-42 --status waiting --limit 20
# CURSOR 取自上一页 next_cursor，过滤条件保持不变。
shipbraid --json list request --task-id issue-42 --status waiting --limit 20 --cursor CURSOR
```

### system、events 与 logs：发现和读取

```text
shipbraid system
shipbraid events --lane <LANE_ID> [--after <SEQUENCE>]
shipbraid logs <RUN_ID> [--after <SEQUENCE>] # v0.3.1 兼容语法
shipbraid logs run <RUN_ID> [--after <SEQUENCE>]
shipbraid logs request <REQUEST_ID> [--cursor <CURSOR>]
```

`system` 返回 API/程序版本、tenant、principal、主体能力和 drain 状态。`events` 读取指定 lane 中严格大于 `--after` 的最多 200 条递增审计事件；默认 after 为 0。继续查询时使用本页最后一个 event sequence；空页保留原值。它只返回一页，没有自动轮询或 `--follow`。

`logs RUN_ID` 是已发布 v0.3.1 的兼容语法。当前源码 `0.3.2-dev` 保留该语法，并新增 `logs run` 和 `logs request`；显式子命令不可与 legacy 根级 run id 混用。后两种 CLI 形式及 request-log HTTP endpoint 需要后续发行包，v0.3.1 不包含。`logs run` 查询固定 run，`--after` 默认 0，响应的 `next_cursor` 是下一次查询所用的数字 sequence。`logs request` 选择请求当前关联的 run，使用不透明 `--cursor`；具体选择顺序、游标切换和空页行为见[HTTP API/CLI 契约](api.md#log-queries)。所有日志命令每次只返回一页，没有 `--follow`。不要仅凭 API v1 推断部署版本支持新语法。

### 写操作与健康检查

```text
shipbraid withdraw <REQUEST_ID>
shipbraid retry <REQUEST_ID>
shipbraid priority <REQUEST_ID> <normal|urgent> [--reason <TEXT>]
shipbraid drain
shipbraid resume
shipbraid resolve-unknown --lane <LANE_ID> \
  --action <confirm-stopped|adopt-running> --evidence <REFERENCE> --reason <TEXT>
shipbraid health
```

`withdraw` 只撤回这个请求，不直接停止其他请求共享的 run。`retry` 使用原目标、参数和截止时间，不能用来换版本或绕过 unknown 所有权核对。`priority` 的原因默认为 `CLI priority update`，服务端原因上限为 2 KiB；提升为 urgent 需要 urgent/admin。`drain` 持久暂停新派发并继续观察活动 run；它不取消 run，也不等待执行排空。`resume` 显式恢复派发，两者均需 admin。`resolve-unknown` 要求 admin、lane 权限和执行器独立观察证据；`--action` 的连字符值会转换成 API 的下划线形式。未证实旧执行停止时不得释放 lane 所有权。`health` 无需 token，只检查 `/ready`，不代表任何请求已满足。

### 机器错误与退出码

带 `--json` 时，CLI 错误写 stdout，形状如下；不要依赖人类可读的 message 来决定恢复动作：

```json
{"error":{"code":"forbidden","message":"The configured token is not authorized for this operation.","retryable":false,"http_status":403}}
```

错误可能含 `http_status`、`retry_after_seconds` 或 `idempotency_key`。常见 code 有 `invalid_arguments`、`invalid_deadline`、`invalid_parameter`、`local_error`、`transport_error` 和 `invalid_response`。network/HTTP 超时可能发生在服务端已接受请求后；submit 恢复时沿用原 key 和正文。`wait` 只会对可重试的读取错误退避，并尊重 Retry-After，不会重新提交。

| 退出码 | 意义 |
| --- | --- |
| 0 | wait 已满足；其他 API 命令成功 |
| 1 | 命令、配置或不可恢复调用错误 |
| 2 | 参数错误，或 wait 观察到 `needs_attention` |
| 3 | wait 观察到请求过期 |
| 4 | wait 观察到请求已撤回 |
| 124 | 客户端本地 wait 超时，服务端请求仍然存在 |

## 验证与边界

运行 `python3 scripts/acceptance-agents.py` 可验证三个独立 Agent 身份共享模拟执行、协作权限、重启恢复与机器错误。全部使用临时仓库、数据库和凭据，`real_deployments=0`。

本轮未实现 MCP、SDK、动态租户、SSE、当前环境新鲜证据复用、真实安全抢占或生产接入；旧性能目标与完整故障矩阵继续由原计划追踪。
