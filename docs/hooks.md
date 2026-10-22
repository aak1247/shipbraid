# 外部执行器协议 v1

调度器只管理请求、所有权与验收，具体执行通过管理员配置的 hook 交接。每条 lane 选择 `scripted` 或 `hooks`；同一个项目的 dev/prod 使用不同 lane、令牌权限和配置指纹。

## 配置与三个传输

```toml
[lanes.executor]
kind = "hooks"
[lanes.executor.dispatch]
kind = "command"
program = "/usr/local/bin/my-ci-bridge"
args = ["dispatch"]
cwd = "/srv/ci-bridge"
timeout_ms = 10000
[lanes.executor.observe]
kind = "script"
path = "/srv/ci-bridge/observe.py"
interpreter = "/usr/bin/python3"
args = []
cwd = "/srv/ci-bridge"
timeout_ms = 10000
[lanes.executor.verify]
kind = "webhook"
url = "https://ci.example.com/hooks/verify"
token_env = "CI_VERIFY_TOKEN"
timeout_ms = 10000
```

这是协议配置示意，不是已经部署的 CI 地址。程序、脚本、参数和 URL 只由服务器配置确定，调用方不能在请求中提供命令。命令/脚本使用固定 argv，没有 shell 字符串插值；JSON 从 stdin 输入、stdout 输出。Webhook POST 相同 JSON，bearer 从环境读取，不跟随重定向；HTTP 只允许 loopback，其余地址要求 HTTPS。

所有 hooks lane 的 `allow_preemption` 必须为 false；v1 不提供 cancel hook。旧发布脚本整个任务按不可抢占处理，不能以 kill 或超时作为停止证明。需要执行任意 shell 脚本时用显式 interpreter + 固定 script path，仍由管理员审查该脚本。

## 信封与回执

请求：

```json
{"version":1,"operation":"dispatch","command":{},"run":{},"lane":{}}
```

`command` 是持久命令（observe 为 null），`run` 为被冻结的执行实体，包含 id、generation、lane_id、target、executor_ref；`lane` 为已授权配置快照。完整字段见 Rust [domain.rs](../src/domain.rs)、[hooks.rs](../src/hooks.rs)。

响应：

```json
{"version":1,"run_id":"<uuid>","generation":1,"executor_ref":"<stable-id>","status":"started","message":"handoff complete","logs":[{"sequence":1,"level":"info","message":"external job started"}]}
```

status 可为 started/running/completed/failed/unknown/verified。started/running 必须有稳定身份；所有响应须匹配 run_id/generation，已有 executor_ref 不得替换。logs 使用该执行内稳定单调的源序号，服务端持久去重并有界暴露。不要在协议消息或日志中返回凭据。

- dispatch 只做快速交接，长构建由独立的持久监督器执行。HTTP 202/传输成功不等于任务已经开始或成功；不明响应会保留通道所有权。
- observe 根据稳定任务身份查询真实状态；completed 仅允许进入 verifying，不满足任何请求。
- verify 是只读、独立、可重新执行的环境观察。只有它的 verified + receipt 才可能满足请求；receipt 的观察时间必须处于这次调用窗口内，远端验证器需时钟同步；receipt 含 id、run_id、generation、observed_at、source_revision、component_digests、profile_digest、verification_report、result。
- 非零退出、超时、错误 JSON、不匹配身份或证据都不能被转换为成功。环境已变化或无法证明停止时保持 unknown。

调用 dispatch 前服务端先持久化不可抢占区间和命令投递意图；重启只观察，绝不盲重发 dispatch。Verify 投递后丢响应可重新只读验收，但仍需新观察与匹配回执。配置指纹变化时不会用新 hook 处理旧 run，需恢复被冻结配置并核对状态。

## 失败后的停止证明与恢复

`failed` 本身不证明部署进程及子任务已经停止。observe/verify 可额外返回：

```json
{"version":1,"run_id":"<uuid>","generation":1,"executor_ref":"<stable-id>","status":"failed","failure_proof":{"observed_at":"<fresh UTC timestamp>","evidence":{"supervisor":"stopped","process_tree":"empty"}}}
```

上例只展示字段形状，不能用静态文本替代真实证据。管理员配置的观察器必须核对完整冻结目标、run/generation、稳定执行器身份及监督器调用身份，独立确认终态和整个进程子树已停止。`observed_at` 必须落在此次调用窗口内；evidence 必须是非空对象，最多 8 KiB。dispatch、不匹配身份、过期证明或带 receipt 的失败证明均被拒绝。完整证据追加到 `failure_stop_verified` 审计事件。

确认后执行变为 `failed`、释放该执行所有权，通道进入 `failure_paused`，相关请求进入 `needs_attention`，由操作者显式重试。它不产生成功 receipt，不满足请求，也不承诺环境没有变化；缺少证明时继续 `unknown` 并保留所有权。

旧冻结观察器无法生成该字段时，可在同一 lane 添加独立的只读失败观察器：

```toml
[lanes.failure_observe]
kind = "script"
path = "/srv/ci-bridge/failure-observe.py"
interpreter = "/usr/bin/python3"
args = []
timeout_ms = 10000
```

该配置支持与其他 hook 相同的 command/script/webhook 传输，必须由管理员审查为只读，输入仍为 `operation=observe`。协调器只接受其 `failed + failure_proof`，忽略其他结果；它不能 dispatch、接纳 running 或验收成功。该补充观察器不改变原执行 profile，原 dispatch/observe/verify 路径和参数必须保持冻结值，目标、generation、所有权检查仍然生效。审计证据记录补充观察器配置的 SHA-256。显式 `confirm_stopped` 也要求重新获取同样的新鲜证据，人工备注不能代替证明。

相同的观察摘要与 unknown 原因只记录一次，去重标记随账本持久化；新状态或原因仍会记录。带新源序号的真实日志即使文字相同也会保存，历史重复审计行不会删除。

## HTTP 与 CLI

```bash
shipbraid --token-file ~/.config/shipbraid/operator.token submit \
  --project myapp --environment dev --revision <full-sha> --components app \
  --idempotency-key <stable-intent-key>
```

HTTP `POST /v1/requests` 同样接受 project + environment，或显式 lane_id（二者互斥）。先解析为唯一已授权 lane，再计算幂等正文；同项目不同环境不会共享执行。存在多个匹配 lane 时必须显式选择 lane_id。完整接口见 [api.md](api.md)。

## 凭据与 hook 环境

从 0.3.2-dev 开始，hooks lane 支持管理员配置专用的部署凭据注入与子进程环境隔离：

```toml
[lanes.credentials]                        # 可选；仅 hooks lane 允许
env_files = ["/private/shipbraid/creds/myapp-dev.env"]

[lanes.hook_environment]                   # 可选；默认 mode = "inherit"
mode = "isolated"                          # inherit | isolated
allow = ["PATH", "LANG", "LC_ALL", "TZ", "HOME", "TMPDIR"]
```

### 隔离语义

- **`inherit`（默认）**：保持向后兼容，hook 子进程（command/script）继承协调器服务进程的完整环境变量。
- **`isolated`**：hook 子进程在执行前执行 `env_clear()` 清空环境，严格仅从协调器进程复制 `allow` 白名单中实际存在的变量，再注入 `credentials` 声明的键值。协调器自身的 API token（如 `SHIPBRAID_TOKEN`）及宿主环境中的未授权敏感变量绝不会泄露给部署子进程。
- **信封不携带凭据**：凭据只通过子进程环境变量提供给 hook 进程，绝对不进入 stdin 的 JSON 请求信封、命令行参数（argv）、tracing 审计日志、回执或数据库持久化存储。Webhook 传输不使用进程注入，继续沿用各自配置的 `token_env`。

### 凭据文件规则

- 凭据文件仅限 hooks lane 配置；scripted 模拟 lane 配置即报错。
- 文件必须是常规文件，Unix 权限禁止 group/other 访问权限位（`mode & 0o077 == 0`，即 0600 或更严格）。
- 单文件上限 64 KiB，全部文件合计不超过 32 个键。
- 格式为字面 `KEY=value` 行，`#` 注释与空行忽略；KEY 须符合 `^[A-Za-z_][A-Za-z0-9_]{0,63}$`；值非空且 ≤ 8 KiB（无 shell 展开）。
- 凭据键不得以 `SHIPBRAID_` 前缀开头，不得与 `allow` 白名单相交，跨文件不得重复。

### 指纹绑定与轮换建议

`effective_profile_digest` 将规范化的 hook 环境配置（模式 + 排序后的 allow）及各凭据文件的路径与内容 SHA-256 共同作为输入。模式变更、白名单变更、凭据文件增减或内容修改均会导致配置指纹变化。

- **轮换建议**：推荐在 lane drain（排空）或空闲期执行凭据轮换。
- **活动 run 语义**：若在执行过程中轮换凭据，活动 run 的后续 `observe` 与 `verify` 调用会按当前配置读取新凭据；由于 `verify` 是只读独立环境验收，这一行为是预期并安全的。

## 二进制安装

发行包包含 `shipbraid` CLI/服务端、Dashboard 静态资源和受支持的适配器。安装与升级步骤见[二进制发行指南](distribution.md)；Rust 与 Node.js 仅在源码构建时需要。已有团队服务时，每个部署边界只运行一个协调器；客户端机器只需二进制、服务地址和自己的 token 文件。

## 通用适配器

每条 lane 使用管理员配置的受信执行器。项目可以提供固定 command/script hooks，也可以提供实现同一版本化 `dispatch`、`observe`、`verify` 协议的 webhook。请求只携带版本、组件和已声明参数，不能选择命令、路径或 URL。

随发行包提供的[本机适配器](../integration/local/README.md)可在 Linux、Python 3.11+、systemd user manager 和 cgroup v2 上监督固定项目命令。示例使用合成项目。真实项目应先在明确授权的非生产环境独立验证适配器配置、凭据、停止证据、实际版本、组件摘要、配置指纹和健康检查，再依赖该执行路径。生产接入需要单独明确授权。

部署脚本和项目专属配置保存在 Shipbraid 仓库之外。接入步骤和项目级 `AGENTS.md` 模板见[项目接入指南](project-integration.md)。
