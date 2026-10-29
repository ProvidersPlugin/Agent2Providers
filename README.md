# Agent2Providers

**English** · [简体中文](README.zh-CN.md)

**Provider Enhancement** (提供商增强) is the official provider plugin for [Agent2API](https://github.com/aimod-cc/Agent2API).
It supplies provider-specific sign-in and account import, model requests, credential
renewal, model catalogs, balances and check-in. Available capabilities vary by provider.
The gateway loads the plugin as a separate process, so a fault in one provider
takes down that process and nothing else.

This repository also carries the two crates the gateway compiles against:
`agent2api-provider-protocol` (the wire types) and `agent2api-provider-sdk`
(the `Provider` trait and its serve loop). The gateway depends on this
repository by tag.

## Supported Providers

Capabilities supplied by Provider Enhancement. The table lists the currently enabled Rust integrations. Static model catalogs, usage and plan information are not equivalent to online catalogs or monetary balances.

| Platform | LLM requests | Token auto-refresh | Model list | Balance query | Check-in | Claims |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| WorkBuddy (China) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| WorkBuddy (Global) | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Raccoon | ✓ (chat / images) | ✓ | ✓ | ✓ | ✓ | — |
| CatPaw | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| AutoClaw (domestic / international) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Qoder | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Cline (Free / Pass) | ✓ | ✓ | ✓ | ✓ | — | — |
| Accio (international / domestic) | ✓ | ✓ | ✓ | ✓ | — | — |
| ZCode (domestic / international) | ✓ | ✗ | ✗ | ✓ | — | ✓ |
| CodeArts | ✓ | ✓ | ✓ | ✓ | — | ✓ |
| Trae | ✓ | ✓ | ✓ | ✓ | — | — |
| Loomy (iFlytek) | ✓ (chat / image generation / reference editing) | ✗ | ✓ | ✓ | ✓ | — |
| KukuAI (Baidu Wenku) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| MonkeyCode (Chaitin, domestic / international) | ✓ | ✗ | ✓ | — | — | — |
| Command Code | ✓ | ✗ | ✓ | — | — | — |
| Antigravity (Google, Gemini / Claude / GPT-OSS) | ✓ (chat / image generation / reference editing) | ✓ | ✓ | — | — | — |
| OfficeAce (Huawei Cloud, OfficeClaw) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| OrcaTerm (Tencent Cloud) | ✓ | ✓ | ✓ | ✓ | — | — |
| ModelScope (MoDa, domestic / international) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| AtomCode (AtomGit / GitCode) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| LobsterAI (NetEase Youdao, Youdao Lobster) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Doubao Work | Implemented; unverified | ✗ | Implemented; unverified | Implemented; unverified | Implemented; unverified | Implemented; unverified |

Custom providers remain a gateway feature and do not require this plugin. They support model requests and model-list fetching; other capabilities depend on the service you configure. API account creation skips a second proxy save only for `proxySaved: true`; older gateways retain a compatibility save and verification warning. Failure does not remove an added account or falsely report that its proxy was saved.

> Note: model-list markers cover online fetching or static catalogs as specified by each row and implementation. Missing refresh or usage hooks do not establish that the upstream lacks an endpoint. Qoder check-in is China-version only.

## Building

```bash
cargo build --release            # current platform
node scripts/pack.mjs            # produce an installable zip (runs the build above)
```

The zip lands in `dist/agent2api-providers-<version>-<target>.zip`.

To build for another target, prepare the toolchain yourself and name it:

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

The script only turns an already-built binary into a zip. It does no cross
compilation — that needs mingw / osxcross / musl toolchains, and folding them
in would add a branch that can only fail. Multiple platforms are the CI
matrix's job (`.github/workflows/build.yml`, one job per platform).

## Every platform needs its own package

A plugin is a native executable that the gateway starts directly, so each
OS-and-architecture pair needs its own binary. `pack.json` declares five:

| Target triple | Covers |
|---|---|
| `x86_64-pc-windows-msvc` | Windows desktop (x64) |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple Silicon |
| `x86_64-unknown-linux-gnu` | Linux desktop amd64, Docker amd64 |
| `aarch64-unknown-linux-gnu` | Docker arm64 |

The gateway checks that the current platform's entry point is in the package
and refuses one that lacks it. Update checks match the asset name
`<id>-<version>-<target>.zip` exactly, so the platform segment cannot be
dropped from the filename.

**Both Linux packages build against the bookworm generation.** The Docker
runtime image is `debian:bookworm-slim` (glibc 2.36). A binary built on a newer
distribution (Ubuntu 24.04, glibc 2.39) requires a newer glibc and fails inside
the container with a dynamic-linker error. CI builds both Linux targets on
Ubuntu 22.04 runners (glibc 2.35), which stays below the runtime image and
therefore runs on it.

## Signing (optional)

```bash
node scripts/sign.mjs keygen                                    # once
node scripts/sign.mjs sign --zip dist/agent2api-providers-*.zip
```

The private key goes to `~/.agent2api/plugin-signing-key.json` by default (or
wherever `--key` / `AGENT2API_PLUGIN_KEY` points) and **never** enters the
repository. Signing is ed25519 over the package contents — id, version,
protocol, target, providers, entry points and the entry binary's sha256 — not
over the zip bytes. After generating a key, put the printed public key into the
gateway's `OFFICIAL_PUBKEYS` (`plugin_host/install.rs`); otherwise the gateway
does not trust it and refuses the install as an invalid signature.

An unsigned package installs and works normally. The interface marks it
"unsigned" in amber, which only means the origin cannot be proven.

## Verifying

```bash
cargo build --release
node scripts/verify.mjs          # defaults to the binary under target/release
node scripts/verify.mjs --exe path/to/agent2api-providers
```

The script acts as the gateway and runs the protocol against the plugin binary:
init, requests, streaming, backpressure, cancellation, callbacks and crash
recovery. All 28 checks must pass.

Every CI platform job runs it — **compiling is not the same as running**. A
missing shared library, a missing executable bit or a wrong path only shows up
at startup, so each platform's binary is started once on its own machine
before the package is produced.

## The two shared crates

- `agent2api-provider-protocol` — the wire types (the contract); the gateway
  and every plugin compile it
- `agent2api-provider-sdk` — the `Provider` trait and the `serve()` loop

Both live under `crates/` here, and `crates/providers/Cargo.toml` refers to
them by relative path inside the workspace. A protocol change takes effect
locally without a release.

The gateway refers to the same repository **by tag**:

```toml
agent2api-provider-protocol = { git = "https://github.com/ProvidersPlugin/Agent2Providers", tag = "v0.2.1" }
```

So a contract change reaches the gateway only after a new tag and an updated
line there. Changing the local copy alone leaves the gateway compiling against
the old contract; a field mismatch may still compile and fail at runtime.

## License

AGPL-3.0
