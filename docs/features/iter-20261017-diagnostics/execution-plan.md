# 只读诊断执行计划

- Status: DONE；源码 0.3.2-dev 已验收，未公开发行。
- Design: [design.md](design.md)
- 目标：发行包用户在启动/提交前获得无秘密、可操作、无副作用的诊断结果。
- 约束：仅单 Cargo package 的诊断模块/CLI/测试/文档/包验收；不改调度器、数据库或真实部署。

| ID | 状态 | 任务 | 依赖 | 完成条件 |
| --- | --- | --- | --- | --- |
| D-001 | DONE | 定义报告模型、CLI 和静态配置入口 | 无 | JSON/文本/退出码契约明确，复用配置解析且不启动 Service/Store |
| D-002 | DONE | token 引用、mirror/ref、hook/程序/目录检查 | D-001 | 正常与缺失用例覆盖，不执行 hook/网络，不修改文件 |
| D-003 | DONE | 显式可选的 user systemd/cgroup 检查 | D-001 | 固定只读探测、超时/输出上限，默认 skipped，不将 remote hooks 误判为依赖本机 systemd |
| D-004 | DONE | 无副作用与秘密 canary 验证 | D-002,D-003 | sentinel 未启动、DB 未创建/打开、配置/mirror 不变、任何输出无秘密 |
| D-005 | DONE | 二进制文档、包验收与正式发行评估 | D-004 | 标准 Rust/协议/包检查通过，记录真实证据，再确定正式发行；不自动升级实例 |

建议顺序 D-001 → D-002/D-003 → D-004 → D-005。一轮规模假设：只复用现有静态校验和受控本地探测，不聚合所有 TOML 语义错误，不实现 active health/repair/upgrade。若需要更改配置格式、调度状态或调用实际执行器，必须移出本轮。

验证命令：`cargo fmt --all --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked`；四套 acceptance 与 distribution 包验收使用刚构建的 binary。本轮验证已通过，见[验收记录](../../acceptance/doctor-20261017.md)与[机器摘要](../../acceptance/doctor-20261017.json)。

退出条件：D-001～D-005 满足且明确报告“检查前提不等于环境验收”；执行 TODO 同步更新。没有实际服务升级或生产部署任务。

D-005 发行评估：本轮保留 `0.3.2-dev`，只生成并验收本机构建包；未创建公开 tag/Release。正式版本仍须走既有 Release 流程，当前实际实例继续使用原发行版。本轮无升级实例任务。
