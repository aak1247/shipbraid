# 单项目 CLI 执行计划

- Status: DONE；本机与发行包验收通过，GitHub checks 在提交后独立确认。
- Design: [design.md](design.md)
- 不创建新 Release，不升级实际实例，不触发真实项目部署。

| ID | 状态 | 负责人 | 任务与完成条件 |
| --- | --- | --- | --- |
| L-001 | DONE | gpt-6-luna/max core | mode 与 local Session；共用 Service/Engine/Store，项目和历史主体边界、独占锁、无 HTTP、受控退出与恢复 |
| L-002 | DONE | gpt-6-luna/max CLI | run 参数、恢复/retry、早于客户端初始化；黑箱验证成功/幂等/冲突/锁/超时/中断/Unknown，机器错误脱敏 |
| L-003 | DONE | gpt-6-luna/max installer/docs | 默认 local 与显式 server 安装、Python 回归、local 样例、binary 文档；不得声称已发布 |
| L-004 | DONE | Codex parent | CLI 接线、架构文档与包级 local/Server 验收集成；审查任务边界和恢复规则 |
| L-005 | DONE | Codex parent | fmt/clippy/test、Python、四套协议与包级验收；真实证据、提交推送与 CI |

验证顺序：L-001/002/003 的独立文件并行实现 → L-004 集成 → L-005。并行代理不运行 Cargo、不暂存或提交，父进程统一检查。

完成后明确区分：本机 CLI 已实现与 GitHub Release 已发行；local 停止调度与部署已停止；请求结果与执行结果。验收失败或不可证明的执行状态不得标为完成。

验收证据：[记录](../../acceptance/local-cli-20261017.md)与[机器摘要](../../acceptance/local-cli-20261017.json)。本轮未发行或变更实际实例。
