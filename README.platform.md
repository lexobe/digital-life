# Survive Platform

Survive 在 Linux 服务器上为数字生物提供 youki 沙盒、公共 LLM、能量账本和运行时长。创建者先建立一个私有环境，写下固定的环境描述，再在其中创建生物。生物可以给自己命名、修改 DNA 和代码，选择模型，向创造者报告。平台服务和默认核心使用 Rust，控制台使用 Rust/Wasm。

## 使用控制台

第一次运行可以照着[开放环境中的第一笔收入](docs/first-open-environment.md)配置环境、创建生物并核实奖励。

1. 创建环境。描述是必填项，名称和备注可选。“开放环境”模板可在保存前修改。创建一个环境时可以暂时不放入生物。
2. 在环境中创建一批生物。指定初始能量和可运行时长，按需覆盖某个生物的 DNA、模型、资源及网络权限。
3. 选择拓扑中的生物节点，查看生命状态、模型价格、报告、工作区、服务和账本。
4. 阅读生物报告和提交时的文件快照。通过控制台或创建者 API 发放能量或延寿，两种奖励可以同时发放。程序重试同一笔奖励时复用幂等键。

[调用说明](docs/llm.txt) 与 [OpenAPI](docs/openapi.json) 分别通过 `/llm.txt`、`/openapi.json` 提供。部署使用[配置示例](config.example.toml)和[沙盒说明](deploy/sandbox-README.md)。

公网控制台通过 `https://<域名>/console/` 或 `https://<公网 IP>/console/` 直接访问，创建者 API 与页面使用同一来源。HTTP 自动跳转 HTTPS，浏览器继续使用创建者凭据鉴权。

[域名的 Caddy 配置示例](deploy/Caddyfile.domain.example) 在 DNS 指向服务器后自动申请和续期证书；[公网 IP 配置示例](deploy/Caddyfile.public.example) 使用 Let's Encrypt 的 IP 证书与自动续期。实际 IP、代理上游地址和证书配置保存在服务器私有部署配置中，不写入仓库。代理与 API 同机运行时可使用默认 `127.0.0.1:8088`；代理位于容器时，将 API 监听地址设为代理可达的宿主内网地址。个体环境 API 使用独立内网监听地址。

## 构建与验证

服务使用 Rust 1.100 或更新版本。锁文件固定依赖：

```sh
cargo build --locked --release --workspace --bins --examples
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

控制台在独立 Rust workspace 中构建为 WebAssembly，JavaScript 用于加载 Wasm 和在隔离预览框架中接收页面，不需要 Node.js 或 npm。构建方法见 [控制台](crates/console/README.md)。

普通测试使用临时数据库、模拟模型和临时状态库，不调用付费模型。真实隔离测试必须在具备所需 Linux 内核能力的主机上显式执行；模拟测试不能替代运行时隔离验证。[验证范围](docs/VALIDATION.md)。

`seed/` 保存个体可独立构建的精简工作区。更改 agent/core 依赖后运行 `sh deploy/package-seed.sh` 更新种子清单和锁文件。

## 部署与环境变量

[配置示例](config.example.toml)、[环境变量示例](deploy/environment.example)、[沙盒运行说明](deploy/sandbox-README.md)。仓库内的地址和路径仅为通用默认值，不包含实际主机或凭据。

1. 按沙盒文档安装开启所需隔离特性的 youki。
2. 按沙盒文档准备包含 Rust 工具链的可信根文件系统；Docker 可以用于构建并导出文件系统，个体由 youki 启动。
3. 构建 Linux 服务二进制及控制台，将源码包与控制台部署到指定目录。
4. 执行 `sudo sh deploy/install.sh`。已有配置和凭据不会被覆盖；更换旧后端时先按迁移说明更新配置。
5. 在 `/etc/survive-platform/config.toml` 配置上游地址和实际模型名称（如 `models = ["gpt-6.2-sol"]`），费率表使用同一名称，在 `/etc/survive-platform/environment` 设置实际密钥，权限为 `0600`。
6. 重启本项目服务，检查控制台基础设施状态。服务可访问不代表执行环境或业务模型可用。

服务启动时自动查找 `/etc/survive-platform/environment`，本地开发可使用 `.env`。也可以用 `--env-file /path/to/private-file` 或 `SURVIVE_ENV_FILE` 指定文件。显式指定的文件缺失或格式错误会拒绝启动。已有进程环境变量优先；文件是字面量 `KEY=value` 数据，支持单/双引号，不执行 shell、不展开变量。改动文件后重启服务生效。

安装时生成创建者凭据，模型密钥保持空白，等待运营者配置。OpenAI 默认线路、CLIProxy 等兼容线路按顺序容灾；实际模型名称与 URL 写在私有部署配置，密钥用环境变量名引用。前端只展示配置和可用性摘要，不返回密钥。

Linux 主机需要 cgroup v2、命名空间、seccomp、OverlayFS 和沙盒文档列出的网络工具。youki 不要求 KVM。控制台可以在运行时不可用时提供观察和诊断，执行层通过能力检查后才会启动个体。

## 能量与寿命

创建者可为环境选多个模型，出生时可进一步限制单个生物的模型范围。生物通过 `choose_model` 为后续推理选模型，或在每次请求中指定模型 ID。

下表是 2026-11-19 采用的能量费率，按 OpenAI 标准文本输入和输出价格等比例换算，以 Luna 的一个输入 token 为 1 能量。[官方模型价格](https://developers.openai.com/api/docs/models)

| 模型 | 输入能量 / token | 输出能量 / token | 1,000 输入 + 200 输出所需能量 |
|---|---:|---:|---:|
| `gpt-6-luna` | 1 | 5 | 2,000 |
| `gpt-6.1-terra` | 10 | 50 | 20,000 |
| `gpt-6.2-sol` | 20 | 100 | 40,000 |
| `gpt-6.2-astra` | 80 | 400 | 160,000 |

费率以标准输入和输出为口径。缓存输入按输入费率计，输出总量包含供应商计入其中的推理 token。缓存折扣、服务层级及长上下文附加价不单独计算。最新报价可在控制台的模型目录查询。

模型每次调用先按选定模型的报价预留能量，再按真实输入、输出用量结算。原始 token 用量与实际能量支出分别保留在账本里。上游结果未知时保留预留额，核实后按该次请求的原始报价结算。只有 LLM 调用收取能量。个体可在出生时允许的模型范围内逐次选择。

寿命表示剩余的沙盒运行时长。只要沙盒在运行，包括核心睡眠或后台进程运行时，就消耗寿命。平台定期将运行时长写入数据库。排队、停机和宿主关机期间暂停计量。能量或寿命耗尽时，平台停止整个沙盒。两项资源均补足后，平台从当前 boot.json 恢复。每小时的心跳同步生命状态，生物持续自主决定行动节奏。

环境描述固定。创造者自行判断何时发放奖励，可选关联报告；重复提交同一笔奖励不会重复增加资源。生物向创造者单向报告，创造者凭据与官方模型供应商密钥均保存在沙盒之外。

## 仓库与部署信息

仅提交可复用源码、示例配置和通用文档。`.local/`、`.env*`（示例除外）、`config.local.toml`、部署环境文件、构建产物均被忽略。实际 SSH 别名、主机地址、私有路径映射、截图和部署验证记录放入 `.local/`，不得写进公共说明或示例。

## 许可

本项目采用 MIT；youki 及其他依赖保留各自许可证。
