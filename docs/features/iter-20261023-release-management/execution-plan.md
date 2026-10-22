# 发布管理入口迭代执行计划

- Status: DONE（2026-10-23）；三特性均通过 开发→独立测试→审核 门禁并提交。F3 曾由测试角色发现 HIGH 缺陷（存量 lane 指纹漂移），退回开发修复后复验通过。
- Design: [design.md](design.md)（v1.1 修订 inherit+credentials 语义与 digest 门禁）
- 角色分工：ZCode 任产品经理与最终审核；gemini-3.8 依次承担开发（T-x1）与测试（T-x2）角色。
- 执行顺序严格串行（共享 config.rs/api.rs/main.rs 与文档，避免同树冲突）：F2 → F1 → F3；每个特性内 开发 → 测试 → 审核提交。
- 不触发真实部署、不新建 Release、不改生产配置；实现必须通过 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`。

| ID | 状态 | 负责人 | 任务与完成条件 |
| --- | --- | --- | --- |
| R-000 | DONE 31ecc19 | ZCode | 本设计与执行计划入库 |
| T-G1 | DONE | gemini-3.8 dev | F2：lane `[lanes.guide]` 配置 + 校验/秘密扫描 + `GET /v1/lanes/{id}/guide`、`GET /v1/guides/{project}` + doctor 探针 + 单元/API 测试 + api.md/project-integration.md/diagnostics.md 更新 |
| T-G2 | DONE | gemini-3.8 test | F2 独立验证：全量三项检查；对抗用例（扫描命中、越权 403、32KiB 边界、components 键校验、digest 不变）；未发现缺陷 |
| T-R1 | DONE cd4ff51 | ZCode | F2 审核 + 小步提交 |
| T-M1 | DONE | gemini-3.8 dev | F1：`src/mcp.rs` + `shipbraid mcp` 子命令 + 14 工具 + 幂等键共享提取（src/client.rs）+ 协议/工具测试 + agents.md/README 更新 |
| T-M2 | DONE | gemini-3.8 test | F1 独立验证：握手两版本、-32601/-32700/-32600、工具面无管理操作负向断言、错误透传、task 幂等键逐字节一致、wait 超时语义；未发现缺陷 |
| T-R2 | DONE 16903dd | ZCode | F1 审核 + 小步提交 |
| T-C1 | DONE | gemini-3.8 dev | F3：credentials/hook_environment 配置校验 + HookExecutor isolated 注入 + effective_profile_digest 绑定 + 适配器 forward_env_prefixes + doctor + 测试 + 文档；含 BUG-F3-001 修复（digest 门禁仅非默认配置参与） |
| T-C2 | DONE | gemini-3.8 test | F3 独立验证：跨进程环境泄漏测试、校验矩阵、digest 变化矩阵与存量兼容断言、适配器 23 项单测；发现 HIGH 缺陷 BUG-F3-001（存量 lane 指纹漂移），退回修复后复验通过 |
| T-R3 | DONE 40e2186 | ZCode | F3 审核 + 设计文档 v1.1 修订 + 小步提交 |
| T-F | DONE | ZCode | 最终审核：干净树全量 cargo 三项检查 + 两个 Python 套件（adapter 23 项、acceptance-agents 8 项 real_deployments=0）+ agents.md 未实现清单核对（MCP 已移除） |

验收纪律：测试角色不得只重跑开发者的测试，必须新增至少一条设计 §9 对应用例；不可证明的行为不得标 DONE；每特性提交信息以 `feat:`/`docs:` 前缀且仅暂存本特性文件。三特性均为可选配置叠加，未配置时行为与升级前一致（含 profile digest 逐字节不变，有测试断言）。
