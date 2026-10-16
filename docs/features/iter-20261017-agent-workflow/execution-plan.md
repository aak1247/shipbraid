# 执行计划：团队 Agent 发布闭环

## 1. Plan Header
- Feature：Shipbraid 团队 Agent 发布闭环；Owner：root；Last updated：2026-10-17。
- Design：[design.md](design.md)；Review：[§13](design.md#13-review-feedback-tracking)。
- Plan status：DONE；Review gate status：PASS。
- Contributors：domain/persistence、API/service、独立 reviewer、root CLI/集成。

## 2. Scope Lock
允许本仓库 domain/store/migration、API/service/config、CLI、isolated tests、文档。不改真实服务/数据库/凭据，不发布、不改 AutoTask、不改 Dashboard；先前品牌文件保留。权限/摘要/覆盖不变量变化需同步设计并评审；PASS 后激活。

## 3. Task Breakdown
### T-201 — local 租户与请求上下文持久化
- Status：DONE；Owner：domain；Priority：P0；Requirements：FR-201/202, NFR-201。
- Objective：新旧状态安全持久化上下文，保留原请求/执行与事务语义。
- Range：src/domain.rs、engine.rs、store.rs、migrations/0002*、必要纯 domain fixture（executor.rs/lineage.rs/tests/ledger_invariants.rs）；不改 service/API/CLI。
- Details：按设计新增 RequestContext、local tenant 默认/校验、Event.actor；补构造点、持久化与新迁移；旧事件不重写。
- Dependencies：独立设计 PASS；与 API worker 协商共享类型。
- Risks：serde 默认、旧迁移 checksum、固定 local 与旧 fixture。
- Evidence：[验证汇总](verification.md)。
- Completion：
  - [x] 旧数据迁移与新上下文重启保留，非 local 拒绝；store migration/context/local tests通过。
  - [x] 旧摘要语义与事务/事件不变量回归通过；store、engine、ledger测试通过。

### T-202 — Agent API、协作授权与发现
- Status：DONE；Owner：api；Priority：P0；Requirements：FR-202~206, NFR-202。
- Objective：统一权限，可发现目标，按任务查询并解释等待。
- Range：src/api.rs、service.rs、config.rs（只在必要时）、API/service tests；不改 domain/store/CLI。
- Details：submit_with_context 兼容包装；同事务变更授权/actor 审计；协作者校验、上下文/hash、actor-specific views、查询分页、system 能力发现。
- Dependencies：设计 PASS、T-201 类型契约；可先并行实现。
- Risks：协作者实际 lane scope、urgent 分权、列表分页兼容。
- Evidence：[验证汇总](verification.md)。
- Completion：
  - [x] 本人/协作者/admin 权限矩阵与跨 lane 拒绝验证；API/service和三主体验收通过。
  - [x] 旧幂等重试与 session-only 兼容、urgent retry 权限、过滤/游标非快照/等待原因/同源动作提示正确；API5项/service7项通过。

### T-203 — Agent CLI 与使用契约
- Status：DONE；Owner：root；Priority：P0；Requirements：FR-207/208。
- Objective：机器可判定结果与错误，任务可找回，等待可恢复。
- Range：src/main.rs、新 CLI integration tests、docs/api.md、docs/agents.md、README（保留品牌改动）。
- Details：全局 --json 错误、context/query flags/system、确定幂等键、wait 瞬态错误有界退避；记录客户端契约而非另建 SDK/MCP。
- Dependencies：设计 PASS、T-202 HTTP 契约。
- Risks：Clap 错误处理、重试不重复副作用、token/URL 泄漏。
- Evidence：[验证汇总](verification.md)。
- Completion：
  - [x] CLI 正常/HTTP/网络/参数错误、确定键及跨 principal 先查再等的恢复流程有测试；CLI9组与Agent8组通过。
  - [x] 文档可复制，无虚构入口或生产能力；docs/agents.md与API契约已更新，CLI流程实际执行通过。

### T-204 — 集成、独立复核与验收
- Status：DONE；Owner：root + domain + reviewer；Priority：P0；Requirements：FR-209, NFR-203。
- Range：scripts/acceptance-agents.py、scripts/acceptance-cli.py、.github/workflows/check.yml、新集成 fixtures、本 feature 文档；必要集成修正。
- Details：至少三个主体并发共享、权限拒绝/允许、withdraw 不影响等待者、重启找回、旧 synthetic 回归；完整 fmt/clippy/test/build 和前端兼容检查。
- Dependencies：T-201~203。
- Risks：不得将合成证明等同真实发布；原性能目标保持未关闭。
- Evidence：[验证汇总](verification.md)。
- Completion：
  - [x] 需要的检查和多 Agent 场景通过，独立实现复核反馈处理完毕；见验证汇总与两份原始摘要。
  - [x] 明确暂未完成的后续工作，源代码分 b492795/b584d55 显式暂存提交，文档与品牌另行提交。

## 4. TODO Board
### TODO
无。
### IN_PROGRESS
无。
### BLOCKED
无用户输入阻塞。
### DONE
- [x] T-201：store 4/4、engine 16/16、ledger invariants 2/2，通过；0001原字节未改。
- [x] T-202：API 4项/service 7项、fmt/clippy通过；独立源码复核无P0/P1，游标P2已修正并独立复核PASS。
- [x] T-203：CLI6个单测/9组HTTP错误夹具通过，任务确定键与恢复流程有Agent验收。
- [x] T-204：49个Rust测试、10个前端测试、原有合成验收/Bridge回归与Agent8组全部通过，review PASS。

## 5. Milestones
契约 PASS → domain/API/CLI 并行实现 → 隔离多 Agent 闭环 → 回归与独立复核。各阶段以完成条件退出，不承诺未经估算的日期。

## 6. Feature-Level Completion Conditions
- [x] T-201~204 DONE 且有可复现证据。
- [x] 最新 review gate PASS、无未解决 P0/P1。
- [x] Rust 必要检查、原合成验收与新多 Agent 验收全部通过。
- [x] 未操作真实发布/生产，旧性能/故障矩阵目标未误关闭。

## 7. Plan Update Rules
root 在状态变化时同步任务/TODO Board/证据；保持实现范围和设计一致，失败不标记 DONE。
