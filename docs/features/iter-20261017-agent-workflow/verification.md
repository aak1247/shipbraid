# 验证记录：团队 Agent 发布闭环

日期：2026-10-17。状态：DONE（仅本迭代范围）。源代码提交：`b492795`（local context/协作 API）与 `b584d55`（Agent CLI/验收脚本/使用契约）。原基线 `22f1810`。本记录不是生产部署或旧 M1 全部目标完成证明。

## 交付与审查

- T-201 domain/persistence：local 固定租户、RequestContext、Event.actor、新迁移与旧数据回放。
- T-202 API/service：事务内统一 owner/collaborator/admin 权限、urgent retry 独立能力、发现/解释/任务查询与分页。
- T-203 root CLI：--json 机器错误、任务与会话参数、规范化目标的确定幂等键、列表恢复及有界只读等待重试。
- T-204 domain + root：三个独立主体的进程验收与 HTTP 错误夹具；原有 Rust/Python/前端回归。
- 独立 reviewer：设计首轮 REVISE 后 v0.2 PASS/HIGH；实现源码复核无 P0/P1。发现的三个 P2 均修正后再次 PASS：长 lane 的游标改固定 UUID 锚点；非 JSON 401/403 等永久错误不重试；默认 task key 规范化有效 lane 与等价时区 deadline。

## 可复现检查

以下命令在本次最终代码上通过：

```bash
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
python3 scripts/acceptance.py
python3 scripts/acceptance-hooks.py
python3 integration/autotask/test_bridge.py
python3 scripts/acceptance-agents.py
python3 scripts/acceptance-cli.py
pnpm --dir web typecheck
pnpm --dir web test
pnpm --dir web build
```

- Rust：41 个库测试 + 6 个 CLI 单测 + 2 个 SQLite 不变量集成测试，共 **49** 个通过。
- 前端：2 个文件共 **10** 个测试通过，typecheck/build 通过；本轮无 Dashboard 修改。
- Bridge：**6** 个 Python tests 通过，没有操作真实 AutoTask 环境。
- 原有 acceptance.py 的 **10** 组场景和 acceptance-hooks.py 的 **4** 组合成 hook 场景通过；后者保持 `synthetic_only=true`，没有真实部署。
- `git diff --check` 通过；`0001_initial.sql` 保持原字节，新增 `0002_local_tenant_context.sql`，旧 schema/JSON/hash/事件迁移回放和非 local 拒绝有实际 SQLite 测试。

## 多 Agent 验收

[原始摘要](agents-acceptance.json) 包含 **8** 组检查，`passed=true`、`synthetic_only=true`、`real_deployments=0`：

1. CLI 发现认证身份、固定 local 租户、能力与组件；跨 lane 拒绝。
2. A/B/C 并发提交同任务仍保留三项独立请求，共享一个 run。A 换 session、等价 lane/project 选择方式及 deadline 时区后重放仍为原请求和首次 session。
3. 未授权 C 无法修改 A；冻结协作者 B 可撤回 A，B/C 请求继续等待。
4. SIGKILL/restart 后 C 按任务查回原请求并 wait；B/C 最终共享同一 receipt，A 保持 withdrawn，B 的实际 actor 审计保留。
5. 任务/主体/状态过滤、limit=1 全页无重复；非法状态、游标和限制被拒绝。
6. 伪造 tenant/principal、未知/executor/跨 lane 协作者被拒绝。
7. 管理员可提升其他主体请求优先级；合成失败后，普通 owner/collaborator 不能 retry urgent，管理员可重试并验收后继执行。
8. 六张实体表 tenant 均为 local，SQLite integrity_check 通过。

## CLI 机器契约

[原始摘要](cli-acceptance.json) 包含 **9** 组检查，`status=passed`、`real_deployments=0`。真实子进程对本地 HTTP fixture 验证：结构化 HTTP 状态与 Retry-After、凭据脱敏、HTML 403 立即失败、503 只读恢复、无观察/零秒等待超时、参数/本地错误不泄漏私有输入、模糊 submit 只发一次且返回原键、help/version 保留文本行为。

本轮没有将新的非终态或历史观察解释为发布成功。CLI 的本地超时不撤回请求；网络失败不作为没有发生部署的证明。

## 交付边界

- 仅修改源代码、迁移、文档与临时合成测试；未安装/重启已有服务，未迁移真实数据库，未修改真实 token，未触发真实发布或生产动作。
- 请求列表默认改为 100 条并返回 next_cursor；客户端应跟随游标。游标不保证跨页快照一致性，动态状态筛选下恢复需从首页重查。
- 同一团队固定 local 是预留字段，不是多租户隔离产品；协作者列表提交时冻结，尚无动态授权管理。
- hooks v1 仍不可抢占。MCP/SDK、SSE、当前环境新鲜证据复用、自动重试、公平策略、完整故障矩阵与原持续负载指标均不在本轮完成范围。
- 原 50 requests/s × 60s、p95 <250ms 目标仍未通过。本次 100 请求正确性突发测试的延迟不可替代原持续负载验收。
