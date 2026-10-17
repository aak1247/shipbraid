# 单项目前台 CLI 与共享 Server

- Date: 2026-10-17
- Status: IMPLEMENTED；本机与发行包验收通过，尚未发行。
- 用户决定：单项目默认 CLI；跨项目协调或需要 Dashboard 时使用 Server。
- 范围：一个 Cargo package、同一请求/调度/账本/执行器内核，两种入口。
- Plan: [execution-plan.md](execution-plan.md)

## 基线与选择

当前源码 0.3.2-dev 的日常 CLI 经 HTTP 调用 Server；只有 doctor 是离线入口。`Service::open` 已负责账本独占锁、配置注册、恢复与持久命令；`scheduler_tick` 可独立于 HTTP 运行。首次安装器只生成 Server 配置、token 和待安装 unit。doctor 已验收，但没有单项目前台发布入口。

| 候选 | VALUE | ALIGN | RISK | FIT | 总分 | 决策 |
| --- | --- | --- | --- | --- | --- | --- |
| 单项目前台 CLI，安装器配套 | 5 | 5 | 4 | 4 | 4.60 | 选择，用户已明确默认体验 |
| 后台 Server up/down 管理 | 3 | 3 | 3 | 4 | 3.20 | 后置；当前 serve 可前台启动 |
| 存量自动升级 plan/apply | 4 | 4 | 2 | 3 | 3.40 | 后置，独立处理数据库与执行所有权 |

## 配置与信任边界

新增顶层 `mode = "local" | "server"`。旧配置省略时仍为 server，保持已有实例行为。新安装器默认 local，Server 安装显式 `--mode server`。

- local：至少一个 lane，所有 lane 属于同一个 project；允许同项目的多个环境。没有 API token 配置，没有 Dashboard 配置。以本机私有账本、配置、脚本与凭据文件的访问权限作为信任边界，稳定主体为 `local-cli`。webhook 的执行器凭据仍按 hook 配置读取。
- server：保留现有 tokens、权限、HTTP 与 Dashboard 行为，可管理多个项目；一个项目也可以显式选择 server。
- `run` 拒绝 server 配置；`serve` 拒绝 local 配置且不打开其账本。doctor 沿用配置自己的 mode 校验。
- local 拒绝包含其他项目或非 local-cli 主体请求的历史账本，且不改写这些请求、run 或配置注册状态；不自动接管共享 Server 账本。模式转换另行处理。
- 两种入口竞争同一规范化数据库锁，第二个进程直接失败；不删除锁文件、强杀持有者或自动换空账本。不同数据库管理同一真实部署目标仍由项目接入维护者避免。

## CLI 契约

```text
shipbraid [--json] run --config PATH [--lane LANE] --revision FULL_SHA --components app[,worker]
    [--coverage exact|descendant] [--priority normal|urgent] [--deadline RFC3339]
    [--reason TEXT] [--idempotency-key KEY] [--task-id ID] [--session-id ID]
    [--param KEY=VALUE] [--timeout-seconds 300]
shipbraid [--json] run --config PATH --resume REQUEST_ID [--retry] [--timeout-seconds 300]
```

只有一个启用 lane 时可省略 `--lane`；多个可选 lane 必须显式选择。恢复不能混入新目标、参数、幂等键或新请求上下文。`--retry` 仅用于恢复，按原重试规则显式重试已证明失败的请求，不能重放 Unknown。完整 SHA、覆盖策略、组件、参数、期限和上下文沿用内核的验证与验收规则。

入口先于 HTTP client 和客户端 token-file 读取，不监听 HTTP、不启动后台进程、不托管 Dashboard。API 的 submit/wait 等既有命令继续服务共享模式，不隐式启动 Server。local 的人工观察入口是命令行报告；需要浏览器时显式配置 server。

创建或恢复后立即在 stderr 输出非秘密 request id/幂等键，最终在 stdout 输出 JSON 报告。报告包含 schema_version、mode、request_id、idempotency_key、reused、request、当前关联 run/receipt、local_wait_timeout/interrupted，以及“本地调度已停止、外部执行可能仍在”的作用域说明；不输出配置或 hook 凭据。只有匹配 receipt 才能报告 satisfied。

未提供 key/task 时使用新随机 key；有 task 时按规范化不可变意图生成稳定 key，session 不参与。已存在的相同 key/正文恢复原请求；不同正文拒绝。历史已完成请求的重放只返回该请求的历史结果，不作为新请求的环境验收。

| 退出码 | 意义 |
| --- | --- |
| 0 | 本请求已满足 |
| 1 | 配置、锁、存储或提交错误 |
| 2 | 参数错误，或请求需要处理/执行 Unknown（结合报告区分） |
| 3 / 4 | 本请求 expired / withdrawn |
| 124 | 本地等待期限到达；停止本地调度，未撤回请求 |
| 130 | 收到中断信号；停止本地调度，未撤回请求 |

## 执行、退出与恢复

run 显式打开同项目账本并恢复该项目的未结状态，调用现有 scheduler；不能为 CLI 另写一套部署或状态机。它会观察账本中同项目其他未结执行，不宣称只打开一个请求便不影响同项目调度。

等待期限为 0 时只创建/恢复并返回当前快照，不启动 scheduler tick；已知 Unknown/needs_attention 返回 2，其他未结等待返回 124，已满足等终态返回原状态码。非零期限进入调度；已满足等终态可立即返回。Unknown 至少获得一次现有只读观察机会，仍无法证明则返回 2，保留所有权。

期限或 SIGINT/SIGTERM 到达时先禁止新 dispatch，停止开启新 tick；等待正在进行的 tick/hook 协议交接按既有有界时限收尾后退出，因而可能超过配置的等待秒数。不把本地超时、中断或 CLI 退出写成执行停止、失败或请求撤回。独立部署必须继续由已有 supervisor 管理；强制进程退出后的不明交接在下次按原规则恢复，不盲目重发。

local 退出后没有后台观察/调度；使用相同 config 与 `--resume REQUEST_ID` 恢复。失败重试是单独的显式 `--retry`；Unknown 不能靠这个开关解除。执行器/部署脚本本身的 Git、Python、systemd 等依赖不会因没有 HTTP Server 而消失；本轮不实现零外部依赖的 Rust 执行器。

## 安装与验收

新安装器 `--mode local`（默认）只生成本机配置、项目 adapter、deploy.env、私有状态目录与 binary 链接，不生成 API tokens 或协调器 unit；`--mode server` 保持原安装输出。发行包包含 local 模拟样例与本机指南，文档示例使用 binary。v0.3.1 没有本轮 mode/run 功能，不能把开发版命令写成已发行功能。

临时配置与镜像验收：无客户端凭据/无可用 HTTP 端口仍可 run；成功、重复 key、冲突、单项目多 lane 选择、超时恢复、中断恢复、失败显式 retry、Unknown 不重复 dispatch、独占锁和跨项目拒绝。local 安装不生成 token/unit，Server 安装回归，包级 local run + doctor 与已有 Server 9 项验收均保留。全部真实项目部署次数为 0。

所需检查：fmt/clippy/test、Python 安装/包/适配器测试、既有四套协议验收、新 CLI 黑箱与发行包验收。不变更现有真实实例、生产授权、数据库 schema 或公开版本。

回退：未发行前可移除本轮入口与安装参数；若已有 local 账本，不删除或替换，保留原 binary/config 与独立执行身份。发现重复部署、未证明停止便释放所有权、跨项目调度或秘密泄漏时阻断发行。

验收证据：[记录](../../acceptance/local-cli-20261017.md)与[机器摘要](../../acceptance/local-cli-20261017.json)。本轮未发行或变更实际实例。
