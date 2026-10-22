# 本机只读诊断

`doctor` 从源码版本 **0.3.2-dev** 开始提供；已发布的 v0.3.1 没有此命令。它可检查 local 或 server 配置，不要求协调器在线，也不需要客户端 token。使用包含该功能的发行包中的二进制：

```bash
shipbraid doctor --config /path/to/config.toml
shipbraid --json doctor --config /path/to/config.toml
# 使用 bundled local adapter 时，额外检查本机依赖：
shipbraid doctor --config /path/to/config.toml --check-local-runtime
```

对 server 配置，由实例维护者在协调器所在主机运行，并提供服务启动时相同的 API/webhook 凭据环境。首次 server 安装生成的 `etc/tokens.env` 只包含协调器身份凭据，可在可信 shell 中加载；其他 webhook 凭据按实例约定设置。仅提供客户端 token 文件不足以检查所有服务身份。local 配置没有 API tokens 或 `tokens.env`，无需加载 token 环境；若配置的 webhook 使用凭据环境变量，仍需按 hook 配置设置它们。

Server 配置示例：

```bash
SHIPBRAID_PREFIX="$HOME/.local/share/shipbraid"
set -a
. "$SHIPBRAID_PREFIX/etc/tokens.env"
set +a
"$SHIPBRAID_PREFIX/bin/shipbraid" doctor \
  --config "$SHIPBRAID_PREFIX/etc/config.toml" --check-local-runtime
```

命令不需要服务在线或客户端身份，不读取 `--token-file`，不请求 `--url`。全局参数仍须满足 CLI 的语法约束，例如两个客户端 token 来源不能同时设置。默认检查配置静态规则、配置引用的凭据和程序/脚本/工作目录，以及本机完整 bare mirror 与 release ref；只有配置了 Dashboard 的 server 模式才检查 Dashboard。可选 runtime 检查查询 user systemd manager 与 cgroup v2；远程 webhook 不因此需要本机 systemd。

`tokens.env` 示例仅适用于 server。local 配置无需 token 环境：

```bash
SHIPBRAID_PREFIX="$HOME/.local/share/shipbraid"
"$SHIPBRAID_PREFIX/bin/shipbraid" doctor --config "$SHIPBRAID_PREFIX/etc/config.toml"
```

检查范围会按配置而异：local 不要求 Dashboard 或 API token；server 会检查已配置的 token、webhook 凭据和 Dashboard；两者都会检查 lane 的本地镜像与 release ref。若 lane 配置了 `[lanes.guide]`，还会对其引用的指引文件（`config.guide_file`）做只读探测：验证文件存在性、常规文件属性、32 KiB 单文件大小上限、UTF-8 编码及秘密模式扫描（PEM 私钥、AWS/Google API Key、GitHub/GitLab/Slack/Anthropic Token 前缀等）；探针仅输出合规元数据与检查状态，绝不泄露文档内容或秘密值。若 lane 配置了 `[lanes.credentials]` 或 `[lanes.hook_environment]`，还会探测凭据文件（`config.credentials_file`）的只读属性（存在性、0600 权限、64 KiB 大小、行格式与键合法性、32 键上限），以及 hook 环境模式（`config.hook_environment`，isolated 摘要及 inherit 风险提示 `warn`），全程绝不暴露凭据值。`--check-local-runtime` 只探测 user systemd 与 cgroup v2；本机 adapter 使用这些能力，纯模拟的 `ScriptedExecutor` 不需要它们。

报告中的 `field` 使用 `lanes[0]`、`tokens[0]` 等配置位置，按 TOML 中的顺序定位。配置值、私有路径、环境变量名、token、hook 参数和子进程原始错误均不输出。解析失败时检查 TOML 和字段类型；静态约束失败时核对必填字段、授权范围、lane 参数与 hook 设置，随后按单项建议修复。命令只检查，不自动修复。

JSON 包含 `schema_version = 1`、二进制 `version`、`ok`、`checks`、`summary` 和 `scope`。每个检查含固定 `code`、`status`（`pass`、`warn`、`fail`、`skipped`）、`field`、`message` 和 `suggestion`。文本与 JSON 对应同一报告。默认 runtime 项为 `skipped`。

| 退出码 | 含义 |
| --- | --- |
| 0 | 所选检查没有失败，可能有 warning 或 skipped |
| 1 | 配置无法读取/解析，或至少一项检查失败 |
| 2 | CLI 使用错误，例如遗漏 `--config` |

诊断不打开、创建、迁移或锁定账本，不执行项目配置的任何 hook，不访问网络或同步 Git，不启停 unit，也不提交发布。Git/systemd 探测有超时和输出上限。文件权限等结果只反映当前账号和检查时刻；报告成功不能代替实际发布后的独立环境验收，也不能证明未知执行已经停止或释放其部署所有权。
