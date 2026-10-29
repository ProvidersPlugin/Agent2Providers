# Agent2Providers

[English](README.md) · **简体中文**

**提供商增强**是 [Agent2API](https://github.com/aimod-cc/Agent2API) 的官方提供商插件，扩展账号登录与导入、模型调用、
凭证续期、模型目录、余额查询和签到等能力，具体功能以各提供商的支持情况为准。
网关把插件作为独立进程加载，因此某个提供商出故障只会让那个进程挂掉，
不会影响网关其他部分。

本仓库同时承载网关编译时所依赖的两个 crate：`agent2api-provider-protocol`
（线上类型）和 `agent2api-provider-sdk`（`Provider` trait 及其 serve 循环）。
网关通过 tag 引用本仓库。

## 支持的提供商

「提供商增强」插件的支持情况一览。下表只列当前启用的 Rust 适配；静态目录、用量和订阅方案信息不等同于在线目录或货币余额。

| 平台 | LLM 请求 | Token 自动续期 | 模型列表 | 余额查询 | 签到 | 领取类 |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| WorkBuddy 国内版 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| WorkBuddy 国际版 | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| 小浣熊 | ✓（对话 / 生图） | ✓ | ✓ | ✓ | ✓ | — |
| CatPaw | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| AutoClaw（国内版 / 国际版） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Qoder | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Cline（Free / Pass） | ✓ | ✓ | ✓ | ✓ | — | — |
| Accio（国际版 / 国内版） | ✓ | ✓ | ✓ | ✓ | — | — |
| ZCode（国内版 / 国际版） | ✓ | ✗ | ✗ | ✓ | — | ✓ |
| CodeArts | ✓ | ✓ | ✓ | ✓ | — | ✓ |
| Trae | ✓ | ✓ | ✓ | ✓ | — | — |
| Loomy（讯飞） | ✓（对话 / 生图 / 参考图编辑） | ✗ | ✓ | ✓ | ✓ | — |
| KukuAI（百度文库） | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| MonkeyCode（长亭科技，国内版 / 国际版） | ✓ | ✗ | ✓ | — | — | — |
| Command Code | ✓ | ✗ | ✓ | — | — | — |
| Antigravity（Google，Gemini / Claude / GPT-OSS） | ✓（对话 / 生图 / 参考图编辑） | ✓ | ✓ | — | — | — |
| OfficeAce（华为云果办 / OfficeClaw） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| OrcaTerm（腾讯云遨驰终端） | ✓ | ✓ | ✓ | ✓ | — | — |
| ModelScope（魔搭，国内版 / 国际版） | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| AtomCode（AtomGit / GitCode） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| LobsterAI（网易有道「有道龙虾」） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| 豆包工作 | 已实现，待实测 | ✗ | 已实现，待实测 | 已实现，待实测 | 已实现，待实测 | 已实现，待实测 |

自定义提供商仍由网关本体提供，无需安装本插件；支持模型请求与模型列表获取，其他能力取决于你配置的服务。API 添加账号只在 `proxySaved: true` 时跳过第二次代理保存；旧网关走兼容保存并保留核查告警，失败不撤销已添加账号，也不假称代理已保存。

> 注：模型列表标记表示在线拉取或静态目录，以该行及实际实现为准。不能仅凭缺少续期或 usage hook 判定上游不存在接口。Qoder 签到仅中国版可用。

## 构建

```bash
cargo build --release            # 当前平台
node scripts/pack.mjs            # 生成可安装的 zip（会先执行上面的构建）
```

zip 输出到 `dist/agent2api-providers-<version>-<target>.zip`。

要构建到其他目标平台，需自行准备工具链并指定目标：

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

该脚本只负责把一个已经构建好的二进制打成 zip，不做交叉编译 —— 那需要
mingw / osxcross / musl 工具链，把它塞进来只会多出一个注定失败的分支。
多平台构建是 CI 矩阵的职责（`.github/workflows/build.yml`，每个平台一个 job）。

## 每个平台都需要自己的包

插件是网关直接启动的原生可执行文件，因此每个操作系统与架构的组合都需要
各自的二进制。`pack.json` 声明了五种：

| 目标三元组 | 覆盖范围 |
|---|---|
| `x86_64-pc-windows-msvc` | Windows 桌面（x64） |
| `x86_64-apple-darwin` | Intel 芯片的 macOS |
| `aarch64-apple-darwin` | Apple Silicon 的 macOS |
| `x86_64-unknown-linux-gnu` | Linux 桌面 amd64、Docker amd64 |
| `aarch64-unknown-linux-gnu` | Docker arm64 |

网关会检查当前平台的入口点是否存在于包中，缺失则拒绝安装。更新检查按资源名
`<id>-<version>-<target>.zip` 精确匹配，所以文件名里的平台段不能省略。

**两个 Linux 包都基于 bookworm 这一代构建。** Docker 运行时镜像是
`debian:bookworm-slim`（glibc 2.36）。在更新的发行版（如 Ubuntu 24.04，
glibc 2.39）上构建出的二进制会要求更新的 glibc，在容器内会以动态链接器报错
的方式失败。CI 在 Ubuntu 22.04 runner（glibc 2.35）上构建两个 Linux 目标，
低于运行时镜像的要求，因此能在其上运行。

## 签名（可选）

```bash
node scripts/sign.mjs keygen                                    # 执行一次
node scripts/sign.mjs sign --zip dist/agent2api-providers-*.zip
```

私钥默认写到 `~/.agent2api/plugin-signing-key.json`（或 `--key` /
`AGENT2API_PLUGIN_KEY` 指定的位置），**绝不**进入仓库。签名是对包内容做
ed25519 签名 —— 包括 id、version、protocol、target、providers、入口点以及
入口二进制的 sha256 —— 而不是对 zip 字节签名。生成密钥后，要把打印出的
公钥放进网关的 `OFFICIAL_PUBKEYS`（`plugin_host/install.rs`）；否则网关
不会信任它，会以签名无效为由拒绝安装。

未签名的包也能正常安装和使用。界面会以琥珀色标注「unsigned」（未签名），
这只表示无法证明来源。

## 验证

```bash
cargo build --release
node scripts/verify.mjs          # 默认使用 target/release 下的二进制
node scripts/verify.mjs --exe path/to/agent2api-providers
```

该脚本扮演网关角色，对插件二进制运行协议：init、请求、流式、背压、取消、
回调以及崩溃恢复。全部 28 项检查必须通过。

CI 的每个平台 job 都会运行它 —— **能编译不等于能运行**。缺失的动态库、
缺失的可执行权限位或错误路径，只有在启动时才会暴露，因此每个平台的二进制
都会先在自己的机器上启动一次，之后才产出包。

## 两个共享 crate

- `agent2api-provider-protocol` —— 线上类型（协议契约）；网关与每个插件都会编译它
- `agent2api-provider-sdk` —— `Provider` trait 与 `serve()` 循环

两者都在本仓库的 `crates/` 下，`crates/providers/Cargo.toml` 通过 workspace
内的相对路径引用它们。协议改动在本地即可生效，无需发布。

网关则以 **tag** 方式引用同一个仓库：

```toml
agent2api-provider-protocol = { git = "https://github.com/ProvidersPlugin/Agent2Providers", tag = "v0.2.1" }
```

因此协议改动只有在打新 tag 并更新网关那行引用之后，才会到达网关。只改本地
副本会让网关继续按旧契约编译；字段不匹配仍可能编译通过，但在运行时失败。

## License

AGPL-3.0
