# 发布管理入口迭代执行计划

- Status: IN_PROGRESS（2026-10-23 启动）。
- Design: [design.md](design.md)
- 角色分工：ZCode 任产品经理与最终审核；gemini-3.8 依次承担开发（T-x1）与测试（T-x2）角色。
- 执行顺序严格串行（共享 config.rs/api.rs/main.rs 与文档，避免同树冲突）：F2 → F1 → F3；每个特性内 开发 → 测试 → 审核提交。
- 不触发真实部署、不新建 Release、不改生产配置；实现必须通过 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`。

| ID | 状态 | 负责人 | 任务与完成条件 |
| --- | --- | --- | --- |
| R-000 | DONE | ZCode | 本设计与执行计划入库 |
| T-G1 | TODO | gemini-3.8 dev | F2：lane `[lanes.guide]` 配置 + 校验/秘密扫描 + `GET /v1/lanes/{id}/guide`、`GET /v1/guides/{project}` + doctor 探针 + 单元/API 测试 + api.md/project-integration.md/diagnostics.md 更新 |
| T-G2 | TODO | gemini-3.8 test | F2 独立验证：全量三项检查；对抗用例（扫描命中、越权 403、32KiB 边界、components 键校验、digest 不变）；输出验收记录 |
| T-R1 | TODO | ZCode | F2 审核 + 小步提交 |
| T-M1 | TODO | gemini-3.8 dev | F1：`src/mcp.rs` + `shipbraid mcp` 子命令 + 14 工具 + 幂等键共享提取 + 协议/工具测试 + agents.md/README 更新（移除"未实现 MCP"） |
| T-M2 | TODO | gemini-3.8 test | F1 独立验证：握手两版本、未知方法 -32601、工具面无管理操作断言、错误透传、task 幂等键一致性、wait 超时语义；acceptance-agents.py 回归 |
| T-R2 | TODO | ZCode | F1 审核 + 小步提交 |
| T-C1 | TODO | gemini-3.8 dev | F3：credentials/hook_environment 配置校验 + HookExecutor isolated 注入 + effective_profile_digest 绑定 + 适配器 forward_env_prefixes + doctor + 测试 + hooks.md/适配器 README/project-integration.md 更新 |
| T-C2 | TODO | gemini-3.8 test | F3 独立验证：子进程环境泄漏测试（marker token 不可见）、权限/大小/重复键校验、digest 变化矩阵、`integration.local.test_adapter`、inherit 默认行为不变回归 |
| T-R3 | TODO | ZCode | F3 审核 + 小步提交 |
| T-F | TODO | ZCode | 最终审核：全量检查 + agents.md 未实现清单核对 + 验收总结 |

验收纪律：测试角色不得只重跑开发者的测试，必须新增至少一条设计 §9 对应用例；不可证明的行为不得标 DONE；每特性提交信息以 `feat:`/`docs:` 前缀且仅暂存本特性文件。
