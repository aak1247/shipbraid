# 下一迭代建议：黑箱实例只读诊断

- Date: 2026-10-17
- Status: PROPOSED；本轮仅规划，尚未实施或发布。
- Scope owner: 项目维护者；执行计划见 [execution-plan.md](execution-plan.md)。

## 目标与现状

让使用发行包的维护者在启动服务或提交部署前，一次获得可分享、无秘密的配置及运行前提检查报告。成功报告表示已检查的前提满足，不表示发布成功、执行器已停止或环境已验收。

已交付：v0.3.1 包含 binary、Dashboard、通用 local/GitHub 适配器和首次安装器；实际升级已有人工验证记录。当前源码 0.3.2-dev 增加 request/run 日志查询与 CLI 参考，尚无新的公开发行包。

证据：

- [CLI](../../../src/main.rs) 的 `health` 只查询 `/ready`，没有离线配置诊断入口。
- [Config::load/validate/resolve_tokens](../../../src/config.rs) 分别处理解析、部分路径/约束和 API token 引用；hook program/interpreter 的非空检查不能证明文件可用。
- [LineageResolver](../../../src/lineage.rs) 在服务启动时检查 bare mirror，release ref 在提交时解析；首次使用者可能到提交时才发现版本线不可解析。
- [local adapter](../../../integration/local/adapter.py) 依赖 user systemd/cgroup v2；[安装器](../../../deploy/install-standalone.py) 不启动服务，也拒绝已有 prefix；[发行指南](../../distribution.md) 的存量升级目前仍是手动流程。
- [M1 验收](../../acceptance/m1.md) 的性能目标仍未通过；[Service::snapshot](../../../src/service.rs) 克隆全状态，历史查询扫描事件。它们是测量候选，不是已经证明的性能根因。

## 候选与优先级

按 VALUE 35%、ALIGN 25%、低依赖风险 20%、单轮可验收性 20%，每项 1–5 分：

| 候选 | VALUE | ALIGN | RISK | FIT | 总分 | 决策 |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 只读配置与主机诊断 | 5 | 5 | 4 | 5 | 4.80 | 本轮建议选择 |
| 容量、启动与历史查询基线 | 4 | 4 | 4 | 4 | 4.00 | 后续单独测量，再选热点 |
| standalone 存量升级工具 | 5 | 5 | 2 | 3 | 4.00 | 诊断完成后再做受控 plan/apply |

本轮不扩展为自动修复、自动升级、HA、调度优化或生产上线。也不把旧 M1 全部未闭项并入这一轮。

## 范围与命令契约

拟新增本机命令：`shipbraid [--json] doctor --config PATH [--check-local-runtime]`。

1. 读取并解析同一套服务配置，沿用路径解析、lane 参数及授权配置校验。先复用现有静态校验；若配置无法解析/校验，返回结构化失败，不为聚合所有错误而复制整个校验器。
2. 检查 API token 环境引用，和 webhook hook 所需的 credential 环境引用。遵循各自运行时规则，不擅自增加新的凭据策略；只输出 present/invalid/missing，不输出值、摘要或长度。
3. 对已解析的 repository_id 检查本地完整 bare mirror 与 release ref。禁止 fetch、网络访问、Git lazy fetch、replace 对象，以及可写 Git 可选锁；复用 lineage 的受控 Git 调用边界。
4. 检查 command/script/interpreter/cwd、Dashboard 静态目录等配置引用。相对 program 按受控 PATH 查找，绝不通过执行程序或 hooks 的 `--help/--version` 来检测它。
5. 仅显式传入 `--check-local-runtime` 时查询 user systemd manager 与 cgroup v2；使用固定、只读、有超时和输出上限的探测。任意 script/webhook 不意味着需要本机 systemd，因此默认不推断或强制此依赖。

报告包含 `schema_version`、`version`、`checks[]`、汇总结论；每项固定 `code`、`status`（pass/warn/fail/skipped）、配置字段定位和修复建议。JSON 不包含完整配置、hook args、token、环境快照、子进程原始 stderr；默认隐藏解析后的私有绝对路径。文本和 JSON 表达相同结果。退出码 0 表示所选检查无失败，1 表示检查失败或使用错误；warning 不作为部署许可。

## 不变量与实现边界

- 不调用 `Service::open`、`Store::open`，不打开、创建、迁移或锁定协调器数据库；不创建配置目录，不启动 scheduler。
- 不执行 dispatch/observe/verify/failure_observe，不发 HTTP 探测，不启停 unit，不修改 mirror/配置/令牌，也不提交发布请求。
- 不要求停止实际实例；不把当前 lane 的 Unknown 解释为 stopped；不释放或修改执行所有权。
- 拟增加一个内部 `diagnostics` 模块，维持单 Cargo package；不增加常驻诊断服务、数据库表或额外凭据文件。
- 先面向 bundled local 使用者与同机配置维护者；远程宿主诊断、主动 webhook 通信和运行中业务健康验收均后置。

## 验收与发布

使用临时配置、bare repositories、合成环境与 sentinel hook。验证缺失/坏 ref、token、不可用程序、可选 runtime probe 及结构化输出；sentinel 不得启动，临时 DB/config/mirror 内容与目录清单前后相同。错误文本中放置秘密 canary，断言任何报告均不泄露它。

Rust fmt/clippy/test、既有四套协议验收必须通过。包级验收新增“无需启动服务即可 doctor”场景，且现有发布/恢复六项继续通过。完成后才决定正式版本号并走既有 GitHub Release 流程；该计划不授权切换 AutoTask 或其他真实服务。

回退：未公开发布前删除独立命令接线和模块即可；无 DB/schema/配置变更。若发现诊断会执行 hook、写 DB/mirror 或输出秘密，立即阻断发行并修复，不以已能启动进程作为通过证据。
