# Agent 发布管理

Shipbraid 接受同一团队内多个 Agent 通过 HTTP 或 CLI 协作。当前租户由服务端固定为 `local`，不接受客户端指定。项目名是 Shipbraid，当前二进制与环境变量仍叫 `auto-deliver`。

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
export AUTO_DELIVER_TOKEN_FILE="$HOME/.config/auto-deliver/agent-a.token"
# 若此前设置过 AUTO_DELIVER_TOKEN，应先 unset，避免两种凭据来源冲突。
auto-deliver --json system
auto-deliver --json list lane
```

`system` 返回 API 版本、`tenant_id`、认证主体与主体能力。通道列出允许的组件、覆盖模式、是否启用、执行器类型和抢占能力；不会暴露镜像路径或凭据。`urgent` 与 `admin` 独立配置，普通 Agent 不需要管理员权限。

## 提交并等待

```bash
REVISION=$(git rev-parse HEAD)
auto-deliver --json submit \
  --project demo --environment test \
  --revision "$REVISION" --components app \
  --task-id issue-42 --session-id session-a1 \
  --collaborator agent-b

# 将 REQUEST_ID 替换为提交结果的 id。
auto-deliver --json wait REQUEST_ID --timeout-seconds 60
```

版本必须已存在于管理员配置的受信镜像并满足发布线规则，CLI 不会根据客户端地址自行拉取仓库。默认 exact；descendant 需要通道显式启用，且 Git 祖先关系不证明功能没有被 revert。

提交结果包含 `id`、`idempotency_key`、`reused`、当前 run、请求状态和 `allowed_actions`。请求、run 与 receipt 是不同实体；最终成功以请求 `satisfied` 和匹配 receipt 为准。

- `task_id`：开发任务关联，多个请求可共享同一任务标识。
- `session_id`：首次创建请求的会话追踪；不授予权限，不影响幂等，重放不会覆盖首次值。
- `collaborators`：有权管理该请求的已配置主体，提交时冻结，最多 16 个；必须已能访问对应通道，不能填 executor 身份。

## 网络不明与会话恢复

带 `--task-id` 且未显式指定幂等键时，CLI 根据任务和规范化提交意图生成确定键。组件、协作者排序去重，会话不参与。CLI 将目标选择解析为同一个 lane，并将截止时间规范为 UTC；保持相同提交意图，同一个 principal 可以跨会话安全重放；改变版本、目标、截止时间、优先级、原因或协作者，会生成新键。显式 `--idempotency-key` 覆盖默认策略。

没有 task 或显式 key 时仍会生成随机键。CLI 在提交成功和提交 HTTP/传输错误的 JSON 中返回 `idempotency_key`，应保留它。遇响应丢失，不可根据网络错误推断请求未受理；使用原键与原正文恢复，不生成新键盲重试。

新 Agent 或丢失本地会话时，先按任务找回：

```bash
auto-deliver --json list request --task-id issue-42 --limit 20
auto-deliver --json inspect request REQUEST_ID
auto-deliver --json wait REQUEST_ID --timeout-seconds 60
```

不同 principal 的同键提交仍是独立请求。后继 Agent 若只接手跟踪，应查询并等待原请求；需要自己独立的上线需求时再提交。兼容请求会共享执行，但不会合并身份、截止时间与最终结果。

列表可按 `--principal agent-a`、`--status waiting`、`--lane demo-test` 过滤。请求列表默认 100 条，最多 200；当 `next_cursor` 非 null 时，用同一组过滤条件传入 `--cursor` 继续。游标不是授权凭证，也不保证跨页状态快照；恢复任务应从首页重新查询。

## 协作权限与状态解释

同一通道的授权 Agent 可以查看请求。撤回、重试和修改优先级要求通道权限，并且是请求本人、冻结的协作者或管理员。提交/提升 urgent，以及 retry 已有 urgent 请求，都另需 urgent 或 admin；撤回 urgent 不需要该额外能力。

```bash
auto-deliver --json withdraw REQUEST_ID
auto-deliver --json retry REQUEST_ID
auto-deliver --json priority REQUEST_ID urgent --reason "incident fix"
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

## 验证与边界

运行 `python3 scripts/acceptance-agents.py` 可验证三个独立 Agent 身份共享模拟执行、协作权限、重启恢复与机器错误。全部使用临时仓库、数据库和凭据，`real_deployments=0`。

本轮未实现 MCP、SDK、动态租户、SSE、当前环境新鲜证据复用、真实安全抢占或生产接入；旧性能目标与完整故障矩阵继续由原计划追踪。
