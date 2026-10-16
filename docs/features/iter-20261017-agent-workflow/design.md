# Shipbraid：团队 Agent 发布闭环

## 1. Document Control
- Feature name：固定 local 租户的团队 Agent 发布闭环。
- Version：0.2；Author：Codex root；Last updated：2026-10-17。
- Reviewers：独立 review 子代理；Status：APPROVED；Review gate status：PASS。
- Linked review report：§13；Linked execution plan：[execution-plan.md](execution-plan.md)。

## 2. Context and Problem
已有独立请求/执行、共享、模拟抢占、hooks 与一次真实开发发布。Agent 缺稳定错误、发现与恢复入口；同 lane 的 retry/priority 权限比 withdraw 宽，协作主体未被完整审计。用户确认先做同一团队，租户固定 local，并授权拆分子代理实施。

## 3. Goals and Non-Goals
Goals：Agent 可发现目标、提交带任务关联和协作者的请求、按任务找回、恢复等待，明确可用操作；跨主体变更显式授权；重启保留关联与证据。
Non-goals：多租户产品、动态账号/授权平台、MCP server、客户端 SDK、多实例、SSE、跨分支合并、当前环境证据复用、自动重试、真实抢占、生产发布和旧性能目标收尾。

## 4. Scope
In scope：domain/store/migrations、config/service/API、CLI、isolated tests、接口文档与本 feature。
Out of scope：真实配置/凭据/服务操作、AutoTask 源码或环境、Dashboard 改版、项目可执行文件重命名；先前 Logo/README 工作保留。
Constraints：一个 Cargo package；所有客户端仍走相同授权服务；旧配置与旧数据默认兼容；外部副作用不进入事务，hooks 继续不可抢占。

## 5. Users and Use Cases
同团队多个 token principal；会话 ID 是追踪元数据，不授予权限。
A/B/C 各提交兼容请求共享 run；A 撤回自己的请求不撤回 B/C。B 仅在 A 明确授予协作者权限或 B 为 admin 时才可变更 A 的请求。Agent 掉线后凭同一任务 ID 找到已有请求继续 wait；无法确认是否受理时用原幂等键重试。

## 6. Requirements
### 6.1 Functional Requirements
- FR-201：服务器固定 tenant_id=local；API 不接收客户端 tenant/principal；旧数据补默认值，非 local 状态拒绝。
- FR-202：不可变请求上下文 task_id、session_id（可选、有界非空字符串）与 collaborators（最多 16 个已配置、能访问此 lane 的非 executor principal）；只有 task_id/collaborators 参与正文语义，collaborators 排序去重；缺省/null/空集合及 session-only 沿用旧幂等摘要。session_id 只在首次创建保存，replay 不覆盖。task/session 限 1..256 bytes、非空且无控制字符。
- FR-203：withdraw/retry/priority 统一要求 lane 权限且为 owner、collaborator 或 admin；提交/提升 urgent、retry 现有 urgent 请求仍需 urgent 或 admin；撤回 urgent 不额外要求 urgent。授权与执行在同一 service 写入事务检查，结构化审计实际 actor；协作者清单首版提交时冻结。
- FR-204：GET /v1/system 返回 tenant_id、api_version、调用者身份与能力；lane view 返回允许组件、仓库 ID、覆盖/抢占等现有能力（不暴露私有路径/凭据）。
- FR-205：request view 增加 task_id/session_id/collaborators、waiting_reason、blocked_reason、allowed_actions（withdraw/retry/raise_priority 提示，与 service 共用 owner/collaborator/admin、lane scope、状态、deadline、manual retry limit、urgent 校验；只代表该时点快照，执行仍重新检查），保留原字段。
- FR-206：GET /v1/requests 增加 task_id/principal/status 过滤、limit 1..200 和 cursor；默认 100，响应 items/next_cursor；稳定顺序 created_at、lane_id、creation_sequence、id 降序，opaque cursor 指向最后条目，不因 offset 漂移产生重复；不承诺跨页快照一致性，时钟回拨/动态状态筛选可能导致新增或漏项，恢复需从首页按 task 重查；非法游标/状态拒绝。lane/read scope 始终应用。
- FR-207：CLI submit 增加 --task-id/--session-id/--collaborator；CLI list request 增加过滤/分页；新增 system 发现入口。--task-id 未提供幂等键时，客户端基于 task+规范化目标（解析 lane_id，截止时间规范化 UTC，含 revision/组件/覆盖/优先级/截止/reason/协作者）生成确定键，会话不参与默认键也不参与幂等正文语义；同意图跨会话可恢复，新的 revision 创建新请求。显式 key 行为保留。任务 ID 用于关联，不是唯一发布意图。跨 principal 不合并请求身份，后继 Agent 必须先按 task 查原请求并 wait；只有原 principal 与原意图的 submit 重放去重。
- FR-208：--json 全局机器模式，业务成功保持原 JSON，错误 stdout 输出 {error:{code,message,retryable,http_status?,retry_after_seconds?,idempotency_key?}}；HTTP 错误不变成不可解析字符串；网络/本地/Clap 参数错误亦结构化，无凭据/URL/body 泄漏；--help/--version 仍为人类可读。既有 wait 退出码保留；wait 遇临时查询失败在本地期限内有界退避、不重提请求；网络模糊 submit 只提示用原键恢复，不盲目重发。
- FR-209：在 isolated fixture 中验证至少三个独立 principal、并发共享、非 owner 拒绝/显式协作者与 admin 允许、urgent 仍分权、撤回不伤其他等待者、关联重启可恢复；实际 hooks/生产不计入本轮验收。
### 6.2 Non-Functional Requirements
- NFR-201：旧 SQLite migration 原文件不改；新迁移加 tenant=local 检查，保留事务与追加事件不变量。旧 API 客户端可忽略新增字段；列表默认分页变更在文档显式说明。
- NFR-202：task/session 仅关联信息，不能作为授权身份；配置之外的协作者拒绝；鉴权/跨 lane scope 不放宽。
- NFR-203：fmt/clippy -D warnings/test/build、现有合成验收、CLI/多 Agent 场景全部通过；不宣称修复旧性能目标或完整 fault matrix。

## 7. Proposed Solution
保留现有架构。domain 新增 LOCAL_TENANT_ID/default_local_tenant 与 RequestContext {task_id:Option<String>,session_id:Option<String>,collaborators:BTreeSet<String>}（Default）。Lane、ReleaseRequest 加 serde default tenant_id；ReleaseRequest 和 SubmitInput 增 context；Event 加可选 actor（旧事件 None）。必要内部构造点统一补默认。所有六个主表增加固定 local 列；request 另有可索引的 task_id，JSON 仍保存完整实体。project 幂等边界由现有 Engine 同项目检查继续保证，不宣称新增多租户隔离。

Service 保留 submit(...) 作为旧内部测试/调用兼容包装，新增 submit_with_context(...原参数..., context:RequestContext)。新增受权 mutation 入口，传已认证 principal/admin/urgent 和可访问 lane 集；同写事务内检查 ownership/collaborators 和权限后调用 engine，engine withdraw 的 owner 参数只能在服务验证后映射，审计记录真正 actor。API hash 在旧 body 上仅在有 task/collaborators 时添加（session 不参与 hash）；JSON 中 session 为创建会话，不在 replay 覆盖。

API /system、lane/request views 随 actor 计算；query 过滤分页沿用内存快照，分页有界但不冒称解决全量 snapshot 性能。cursor 只传版本与请求 UUID，服务器按可见的 append-only 请求还原不可变排序锚点；不可见/不存在统一拒绝，作为不可信输入严格解析。CLI 直接复用 HTTP，不另起状态数据库；任务可通过 list request --task-id 恢复。机器协议文档列出字段、错误与恢复流程；本轮不实现完整 OpenAPI 或 SDK。

UI 无改动。现有 token config 默认兼容；协作者由现有 token id 指定。升级离线迁移仅用于隔离数据库测试，本轮不安装到真实服务。

## 8. Alternatives and Tradeoffs
拒绝共享管理员 token（丢身份/过宽）；拒绝客户端自报 principal（可伪造）；不引入复杂 delegation grant 系统，冻结 collaborators 足够本轮。恢复用服务器任务索引查询而非本地 session 文件；换机器仍可找回。MCP 留后续，避免维护两套权限。

## 9. Risk Assessment
摘要变更破坏旧幂等重试→空上下文保留旧摘要，session 不进摘要。新列与 JSON 不一致→默认反序列化、固定 local 校验和迁移回归。协作混淆→owner/collaborator/admin 同事务检查并审计 actor；跨 lane 仍拒绝。列表截断→明确 next_cursor 和文档。回滚代码版本前应使用迁移前在线备份；本轮只迁移临时数据库，不操作活动服务。

## 10. Validation Strategy
domain/store 单测覆盖默认/非 local、关联持久化、旧数据迁移、幂等兼容；API tests 覆盖 auth 和非法输入/分页；独立 CLI 场景模拟 HTTP 错误/超时和多 Agent 共用真实本地 SQLite。运行所要求的 Rust 检查及 acceptance.py/acceptance-hooks.py。若 API 形状兼容，现有前端 typecheck/tests/build 作为回归，不改 UI。
Acceptance：FR-201~209、NFR-201~203 全部有证据；三个 principal 两种授权角色与 admin、请求终态证据正确；无真实发布。

## 11. Rollout Plan
本地实现→隔离迁移/权限/CLI 验收→完整回归→代码小步提交。生产与既有常驻服务保持原样。功能按兼容新增字段默认生效；无动态租户开关。发现破坏请求账本/权限/幂等时停止交付修复，不修改真实数据库回滚。

## 12. Open Questions
用户已决定同一团队、local 租户。首版默认同 lane 可读、跨主体变更须冻结协作者或 admin；session 仅追踪；跨 Agent 各自请求不合并身份。无必须等待用户的阻塞问题。

## 13. Review Feedback Tracking
独立 reviewer 首轮 REVISE，置信度 HIGH；范围适合单迭代。DG-001 session 摘要冲突、DG-002 urgent retry 能力、DG-003 动作提示同源、DG-004 分页非快照、DG-005 跨 principal 恢复边界均已写入 §6/§7。实现必须保持旧迁移/摘要/事件不变，并在 writer 中重新授权。独立 reviewer 对 v0.2 复核 PASS/HIGH，无剩余必要设计修正。独立实现复核未发现 P0/P1；三项 P2（长 lane 游标、非 JSON 永久错误重试、默认键 selector/deadline 规范化）已修正并复核 PASS，相关场景纳入 API 与 CLI/多 Agent 验收。

## 14. Definition of Design Complete
- [x] 目标、范围、用户决定、接口草案与风险明确。
- [x] 要求可追踪，验证和回退边界明确。
- [x] 独立 review PASS，反馈已纳入。
