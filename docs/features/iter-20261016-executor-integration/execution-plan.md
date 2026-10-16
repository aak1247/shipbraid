# 执行计划：可配置执行器与 AutoTask 接入

## 1. Plan Header
- Owner: Codex；子代理沿用用户指定 gpt-6-luna/max。
- Design: [design.md](design.md)；Last updated: 2026-10-16。
- Plan status: ACTIVE；Review gate: PASS（独立复核确认修订后的设计无阻塞）。

## 2. Scope Lock
只修改 auto-deliver 工程与 AutoTask 的发布提交入口/文档。首个真实目标本机开发环境；不变更生产、不覆盖其他会话工作、不重写构建系统。新增能力遵守原覆盖、持久化、unknown 和验收不变量。

## 3. Task Breakdown
### T-101 可配置 hook 与真实执行生命周期
- Status: IN_PROGRESS；Owner: backend；Priority: P0；Requirements: FR-101/103/104/105, NFR-101/102/103。
- Range: src/config.rs、executor.rs、新 hook 模块、service.rs、api.rs、migrations（如必要）。
- Dependencies: 技术评审 PASS。
- Details: command/script/webhook 共用 JSON 协议；异步有界调用、独立观察和 verify、重启不盲重放、真实模式不再自动生成模拟回执；提供执行器 metadata。
- Completion: 三传输隔离进程验收；超时/失败/重放/配置重启 fence 正确；fmt/clippy/test 通过。

### T-102 环境选择、Dashboard 与常驻包装
- Status: IN_PROGRESS；Owner: root + dashboard；Priority: P1；Requirements: FR-102/106。
- Range: src/main.rs、web、Cargo 清单、部署 unit/config/说明；API/config 改动与 backend 协调。
- Dependencies: T-101 契约。
- Completion: HTTP/CLI project/environment 解析且 lane_id 兼容；歧义失败；静态同源页面与鉴权真实 API 可用；前端检查和真实浏览器通过。

### T-103 AutoTask 桥接和提交入口
- Status: IN_PROGRESS；Owner: root；Priority: P0；Requirements: FR-107, NFR-102。
- Range: integration/autotask/、AutoTask scripts/deploy-coordinated.*、相应说明与测试。
- Dependencies: T-101 协议；本机开发环境已明确。
- Completion: systemd 稳定任务身份；以冻结 SHA 发布；独立校验实际进程哈希/源码/健康；AutoTask 包装器 HTTP 提交+等待；代码合入/push vibe。

### T-104 回归、部署与真实请求验收
- Status: IN_PROGRESS；Owner: root；Priority: P0；Requirements: 所有上述 FR/NFR。
- Range: tests/scripts、docs 验收、用户级 auto-deliver unit 与私有配置/数据。
- Dependencies: T-101~103。
- Completion: 运行必要质量检查、部署服务、真实开发发布完成并有匹配 receipt、Dashboard 实际数据可见、提交推送和状态记录。生产保持禁用。

## 4. TODO Board
- TODO: 无。
- IN_PROGRESS: T-101、T-102、T-103、T-104。
- BLOCKED: 无；环境已明确。
- DONE: 无。

## 5. Milestones
协议及 isolated tests → 常驻服务+提交入口 → 一次真实开发发布及 Dashboard 验收。

## 6. Feature-Level Completion Conditions
全部必要任务 DONE、有测试及真实运行证据、技术 gate PASS、两仓库推送成功。M1 性能/完整故障矩阵未完成项不因本迭代被关闭。

## 7. Plan Update Rules
状态与证据同步；越界先修订设计；生产变更需另有明确目标与授权。新执行机制不得虚构安全停止或环境验收。
