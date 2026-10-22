# 发布管理入口迭代设计：项目指引、MCP 与凭据注入

## 1. Document Control

- Feature name：agent 发布管理三件套——自然语言发布指引（F2）、MCP 服务（F1）、凭据管理与 hook 环境注入（F3）。
- Version：1.1，2026-10-23。修订：§7.2 明确 inherit 模式下显式配置 credentials 时注入凭据（存量未配置 lane 零影响）；§7.3 明确空/默认配置不参与 digest。
- Author：ZCode（产品与技术设计）；开发与测试由 gemini-3.8 承担，最终审核 ZCode。
- Status：APPROVED（本轮范围如 §4；不构成生产接入授权）。
- Linked execution plan：[execution-plan.md](execution-plan.md)。
- 上游设计：[docs/design.md](../../design.md) §7.8/§7.10；[docs/hooks.md](../../hooks.md)；[docs/agents.md](../../agents.md)。

## 2. Context and Problem

当前 agent 通过 CLI + HTTP API 使用 shipbraid，存在三个缺口：

1. 项目接入知识（怎么发、哪个环境有什么特殊流程、模块注意事项）散落在各项目 `AGENTS.md` 的手抄模板里，没有权威来源，也不能随 lane 配置一起管理。
2. agent 宿主（Claude Code、Codex、ZCode 等）原生消费 MCP；CLI 封装需要 agent 自己拼 shell，错误恢复语义（幂等键、task 恢复）容易丢。docs/agents.md 已声明 MCP 未实现。
3. 发布脚本的凭据目前只能由执行侧自行解决（本机适配器的 `environment_file` 或 webhook `token_env`）；协调器直接派生的 command/script hook 子进程继承服务进程全部环境，包含 `token_env` 读入的 agent token 值（src/hooks.rs `Command::new` 未做环境隔离）。应用仓库为了跑发布脚本往往把凭据写进 git，违反项目不变量。

## 3. Goals and Non-Goals

**Goals**

- F2：lane 绑定的三级自然语言发布指引（项目总述 / 环境 / 组件），管理员文件维护，API 读取，含内容摘要与秘密扫描。
- F1：`shipbraid mcp` stdio 服务，把现有 API 的 agent 安全子集暴露为 MCP 工具；不新建鉴权模型。
- F3：凭据以 lane 级引用文件管理；hook 进程可选环境隔离（白名单 + 注入）；凭据版本进入 profile 指纹；修复环境继承缺口（opt-in 方式，不破坏存量 lane）。

**Non-Goals**

- 不做加密凭据库（SQLite 存密、KMS、主密钥轮换）——列为后续第二档。
- 不做 MCP HTTP/streamable transport、资源订阅、提示（prompts）面。
- 不把指引或凭据内容写进请求目标、幂等哈希、receipt 或数据库明文。
- 不改 hooks v1 信封格式；秘密不进入 JSON 信封。
- 不做 local 模式（前台 `run` 独占账本）的 MCP 适配；MCP 只面向 server 模式。
- 不触碰真实生产环境；scripted/hooks 执行边界不变。

## 4. Scope

| 阶段 | 包含 | 不包含 |
| --- | --- | --- |
| F2 指引 | lane 配置引用三个 markdown 文件；`GET /v1/lanes/{id}/guide`、`GET /v1/guides/{project}`；加载期秘密扫描与大小限制；doctor 检查 | 指引版本写入请求审计；Web UI 编辑器 |
| F1 MCP | `shipbraid mcp` 子命令；stdio JSON-RPC 2.0；initialize/tools 协议；14 个只读+写工具；管理操作不进工具面 | HTTP transport；resources/prompts 强制面；SSE |
| F3 凭据 | `[lanes.credentials]` env 文件引用；`[lanes.hook_environment]` inherit/isolated；command/script hook 注入；profile 指纹绑定；适配器 `forward_env_prefixes`；doctor 检查 | 加密存储；webhook 凭据改造；执行侧多主机分发 |

三段全部为**可选配置 + 默认不变**：不配指引、不配凭据、不选 isolated 的存量配置行为与升级前完全一致。本轮**无数据库 schema 变更、无迁移**。

## 5. F2 方案：项目发布指引

### 5.1 配置

lane 内新增可选段（`deny_unknown_fields` 风格与现有 LaneConfig 一致）：

```toml
[[lanes]]
id = "myapp-dev"
# ...现有字段不变...
[lanes.guide]                                   # 整段可选；三个键均可独立缺省
project = "guides/myapp/overview.md"            # 项目总述：发布线、镜像同步、审批边界
environment = "guides/myapp/dev.md"             # 本环境流程、参数要求、注意事项
[lanes.guide.components]
app = "guides/myapp/app.md"                     # 组件单独说明，键必须是 lane.components 成员
worker = "guides/myapp/worker.md"
```

路径规则同其他 lane 路径：相对路径按配置文件目录解析；必须存在且为常规文件。

### 5.2 校验与秘密扫描

- 每文件 ≤ 32 KiB（`MAX_GUIDE_FILE_BYTES = 32768`）。
- `guide.components` 的键必须 ⊆ `lane.components`，不得重复。
- 加载期（`Config::validate`）对内容做秘密模式扫描，命中即校验失败、拒绝启动（fail closed）。模式集合（固定列表，避免高误报的泛化规则）：
  - PEM 私钥头 `-----BEGIN ... PRIVATE KEY-----`；
  - `AKIA[0-9A-Z]{16}`、`AIza[0-9A-Za-z_\-]{35}`；
  - `ghp_`、`gho_`、`ghu_`、`ghs_`、`github_pat_`、`glpat-`、`xox[baprs]-`、`sk-ant-` 前缀后跟 ≥16 位非空白。
- 扫描同样运行在 doctor（configuration 探针）中作为只读报告。
- 指引**不参与** `effective_profile_digest`、幂等哈希与请求冻结：它是建议性文档，不是 target 语义。响应携带内容 SHA-256 供调用方审计"读到的是哪一版"。

### 5.3 API

`GET /v1/lanes/{id}/guide`——鉴权与 `GET /v1/lanes/{id}` 相同（token 需该 lane 读权限）：

```json
{
  "lane_id": "myapp-dev",
  "project": "myapp",
  "environment": "dev",
  "sections": [
    {"scope": "project",     "name": "project", "content_sha256": "...", "content": "# ..."},
    {"scope": "environment", "name": "dev",     "content_sha256": "...", "content": "..."},
    {"scope": "component",   "name": "app",     "content_sha256": "...", "content": "..."}
  ]
}
```

未配置的层级不出现在 `sections`；组件按 lane.components 声明顺序排列。

`GET /v1/guides/{project}`——聚合该 token 可见 lane 的指引，供离线阅读与 MCP：

```json
{
  "project": "myapp",
  "overview": {"content_sha256": "...", "content": "..."},
  "environments": [{"environment": "dev", "lane_ids": ["myapp-dev"], "content_sha256": "...", "content": "..."}],
  "components":  [{"component": "app", "content_sha256": "...", "content": "..."}]
}
```

`overview` 无配置时为 `null`；environments/components 按内容文件路径去重（多个 lane 共享同一文件只出一条）。token 在该项目下无任何可见 lane 时返回 403 `forbidden`。错误形状沿用现有 `{code,message,retryable}`。

### 5.4 文档

更新 docs/api.md（两个端点）、docs/project-integration.md（指引取代手抄 AGENTS.md 模板的说明，保留模板作为兜底）、docs/diagnostics.md（doctor 项）。

## 6. F1 方案：MCP 服务

### 6.1 形态与传输

`shipbraid mcp` 子命令：stdio transport，JSON-RPC 2.0、换行分隔（一行一个消息），请求读自 stdin、响应写 stdout、诊断只写 stderr。复用全局参数 `--url`（默认与 CLI 相同）与 `--token`/`--token-file`/`SHIPBRAID_TOKEN*` 环境；不新增配置文件或凭据来源。协议实现**手写最小集**，不引入新依赖：

- `initialize` → 协商 `protocolVersion`（支持 `2025-06-18` 与 `2024-11-05`，客户端给未知版本时回服务端支持的版本）；返回 `serverInfo: {name: "shipbraid-mcp", version: CARGO_PKG_VERSION}`、`capabilities: {tools: {}}`。
- `notifications/initialized` 等通知：静默忽略。
- `tools/list` → 工具清单（名称、描述、JSON-Schema 形状的 inputSchema）。
- `tools/call` → 执行并返回 `{content: [{type: "text", text: <API JSON>}], isError?}`；API 错误原样透传为 `isError: true` 的 text content（保留结构化 code/retryable，不解析人类文本）。
- 其他方法 → JSON-RPC `-32601`。MCP 会话不缓存请求状态。

### 6.2 工具面

只读：`system`、`list_lanes`、`inspect_lane`、`get_release_guide`（参数 `lane_id` 或 `project`，直连 §5.3 端点）、`get_request`、`list_requests`（支持 task_id/principal/status/lane 过滤与 cursor）、`get_run`、`get_events`、`get_logs`（run 或 request 模式）。
写操作：`submit_request`、`wait_request`、`withdraw_request`、`retry_request`、`raise_priority`。

约束：

- `submit_request` 参数与 CLI submit 一致（lane 或 project+environment、revision、components、coverage、priority、deadline_at、reason、task_id、session_id、collaborators、parameters）。携带 `task_id` 时**必须**复用 CLI 既有的任务确定性幂等键推导（从 main.rs 提取共享函数，不复制实现）；无 task 时生成随机 UUID 键并在结果中回显。
- `wait_request(request_id, timeout_seconds=300, poll_seconds=0.5)`：本地有界轮询，timeout 上限 600 秒；终态或超时返回请求 JSON，超时附 `local_wait_timeout: true`。语义与 CLI `wait` 对齐：超时≠撤回。
- **管理操作不进工具面**：drain/resume/unpause/resolve-unknown/resources 一律不提供，即使 token 有 admin 能力（能力仍在服务端按 token 校验，MCP 不放大权限也不隐藏权限）。
- 工具描述文本里写明边界：202 ≠ 发布完成；成功以请求 `satisfied` + 匹配 receipt 为准；unknown 不解锁。

### 6.3 实现结构

新模块 `src/mcp.rs`：JSON-RPC 帧解析、方法分发表、工具 schema 常量；API 调用复用 main.rs 现有 reqwest 客户端层（必要时将其抽出为可共享函数）。`main.rs` 增加 `Mcp` 子命令（无额外必需参数）。测试：协议帧/握手/错误码单元测试 + 工具清单与 API 端点映射测试（HTTP 层用现有测试手法 mock/直连 axum）。

### 6.4 文档

docs/agents.md 新增 "MCP" 一节（工具清单、配置示例、恢复语义、管理操作边界），并把"本轮未实现 MCP"从未实现清单移除；README/README.zh-CN 提及一段；docs/api.md 不变（MCP 是客户端）。

## 7. F3 方案：凭据管理与 hook 环境注入

### 7.1 配置

```toml
[[lanes]]
id = "myapp-dev"
# ...
[lanes.credentials]                        # 可选；仅 hooks lane 允许
env_files = ["/private/shipbraid/creds/myapp-dev.env"]

[lanes.hook_environment]                   # 可选；默认 mode = "inherit"
mode = "isolated"                          # inherit | isolated
allow = ["PATH", "LANG", "LC_ALL", "TZ", "HOME", "TMPDIR"]
```

env 文件格式（与本机适配器 environment_file 一致）：`KEY=value` 字面行，`#` 注释与空行忽略；KEY 匹配 `^[A-Za-z_][A-Za-z0-9_]{0,63}$`；值非空、≤ 8 KiB、取首个 `=` 之后原文（剥离行尾 `\r`），无 shell 展开。

校验（全部 fail closed，`Config::validate`）：

- 仅 hooks lane 可配置 `credentials`；scripted lane 配置即报错。
- 文件必须存在、常规文件、unix 权限无 group/other 位（`mode & 0o077 == 0`）、单文件 ≤ 64 KiB、全部文件合计 ≤ 32 个键。
- 键跨文件重复、键与 `allow` 白名单或 `SHIPBRAID_` 前缀保留名冲突 → 报错。
- `mode = "isolated"` 允许的键集合与凭据键不相交。

### 7.2 注入机制

`HookExecutor::call` 对 `command`/`script` 两类 hook（dispatch/observe/verify 及 failure_observe 全部生效）：

- `mode = "inherit"`（默认）：继续继承服务进程环境。存量 lane（未配置 `credentials`）行为与升级前完全一致；管理员显式配置了 `[lanes.credentials]` 时，在继承环境之上追加注入凭据（渐进采用路径，doctor 会对 inherit 模式给出风险提示）。
- `mode = "isolated"`：`Command::env_clear()` 后，先从协调器进程复制 `allow` 白名单中实际存在的变量，再注入凭据键值。webhook 传输不变（沿用 `token_env`）。

秘密只存在于子进程环境；不进入 JSON 信封、argv、日志、回执与数据库。本机适配器新增可选配置 `forward_env_prefixes: []`（默认空）：适配器把自身进程环境中匹配前缀的变量（受同样的键名/大小规则约束）并入 systemd worker 环境，与既有 `environment_file` 叠加；协调器侧 isolated 注入 + 适配器转发即可让部署脚本拿到凭据，全程不经信封。

### 7.3 profile 指纹绑定与轮换

`effective_profile_digest` 追加两项输入（序列化方式与 request_parameters 一致，即**空/默认配置不参与哈希**，保证存量 lane 指纹与升级前逐字节一致）：

1. `hook_environment` 的规范化序列化（mode、排序后的 allow）；
2. 凭据文件集：每个文件的路径 + 内容 SHA-256（按路径排序）。

效果：mode/allow/文件集合/文件内容任一变化 → digest 变化，与 §7.10 "秘密版本引用" 的既有设计对齐；旧 run 冻结旧 profile 的语义不变。轮换操作建议在 drain 或 lane 空闲时进行（活动 run 的 observe/verify 会用当前配置读取新凭据；verify 只读，可接受，文档写明）。

### 7.4 修复的缺口与 doctor

isolated 模式即是 `src/hooks.rs` 环境继承缺口的修复载体：默认不强制，避免破坏依赖继承环境的存量 hook；文档与 doctor 明确提示 inherit 的风险。doctor（configuration 探针）新增：凭据文件存在性/权限/可解析性、hook_environment 模式摘要、指引文件存在性/大小/扫描结果——只报告元数据，不输出内容。

### 7.5 文档

docs/hooks.md（凭据与 hook 环境新节 + 轮换操作指引）、integration/local/README.md（forward_env_prefixes）、docs/project-integration.md（接入表增行）、docs/agents.md（凭据边界一句话）、docs/diagnostics.md（doctor 项）。

## 8. Risks

| 风险 | 缓解 |
| --- | --- |
| 指引被写入秘密 | 加载期 fail-closed 扫描 + 文档禁止 + doctor 报告 |
| MCP 工具面扩大攻击面 | 只映射现有 API；服务端逐调用鉴权；管理操作不入面 |
| isolated 破坏存量 hook | 默认 inherit；isolated 显式 opt-in；digest 变化可见 |
| 凭据经日志/信封泄漏 | 秘密只走子进程环境；信封/日志/回执不变；测试断言 |
| 轮换时活动 run 行为含混 | digest 绑定 + drain 期轮换指引 + 冻结语义测试 |
| 手写 MCP 协议偏差 | 固定两个协议版本、帧级单元测试、错误码契约测试 |

## 9. Validation Strategy

- F2：秘密扫描命中/放行用例；大小与组件键校验；两端点鉴权（无 lane 权限 → 403）；未配置 lane 的 sections 为空；digest 不因指引变化而变化。
- F1：initialize 握手（两个协议版本、未知版本回退）；tools/list 固定快照（含"无管理工具"断言）；tools/call 错误透传（isError + 结构化 code）；task_id 幂等键与 CLI 推导一致；wait 超时语义。
- F3：isolated hook 子进程环境测试（marker token 不可见、凭据可见、白名单变量可见、白名单外变量不可见）；env 文件权限/大小/重复键/保留名冲突校验；内容变更 → digest 变化、mode 变更 → digest 变化；适配器 python 单测（前缀转发、键名规则、与 environment_file 叠加）。
- 全局：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`；`python3 -m unittest integration.local.test_adapter`；`python3 scripts/acceptance-agents.py` 回归（agent 协作不受影响）。

## 10. Rollout

全部能力为可选配置叠加，不配即无行为变化；无 DB 迁移；发行说明按 features 分别标注。生产 lane 接入凭据注入仍需单独授权与环境验收，测试 lane 先行。

## 11. Open Questions

- 指引版本是否记入请求审计（guide_version 字段）：本轮不做，评审后定。
- 加密凭据存储（第二档）：多主机/高频轮换需求出现时立项。
- MCP streamable HTTP transport 与资源模板：后续按宿主需求加。
