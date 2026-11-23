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

Capabilities supplied by Provider Enhancement. The table describes the existing native integrations, not completion of the WASM development packages. Each WASM package contains one provider identity and is unsigned and unverified; regions and quota pools do not share account access. Static model catalogs, usage and plan information are not equivalent to online catalogs or monetary balances.

WASM-specific status: the former foundation packages now include CodeArts chat, official sign-in, host-managed renewal, daily benefits and the new-user gift; CatPaw tool continuation, image input and authorized live desktop sign-in; Doubao Work chat, asynchronous replies, image upload/generation, browser sign-in, desktop import, check-in and completed-task rewards; and separate MonkeyCode China/International chat tasks, client-tool bridges, sign-in and daily check-in. MonkeyCode tasks and desktop credential reads require explicit permission. Kiro managed sign-in and standalone Cline Free renewal remain available (Cline Pass uses the gateway API template instead of a separate WASM package). These implementations are not live-account verified.

Raccoon WASM retains manual and proactive renewal for ordinary accounts and restores Refresh sign-in for linked local accounts. Local Kiro IDE / CLI, AutoClaw China / International and Cline Free accounts can renew and safely write back after separate authorization; without it they remain read-only. Raccoon IAM credentials cannot use the legacy refresh endpoint and still require signing in again. One-time refresh material is durably reserved before sending, compared before saving and read back afterwards. Uncertain results never replay old material, and a memory overlay is not persistent write-back. These paths are implemented but unverified.

WASM migration additions:

- Imported Kiro external identity credentials can renew through a controlled host entry when they retain the refresh token, client ID and official token endpoint. Ordinary network permissions do not authorize arbitrary origins. Social renewal retains the official US and European sources; AWS enterprise authentication uses an exact commercial-region list, separately from the subscription's business region.
- The host parses legacy JSON-string credentials within the current call while retaining the exact original text for conditional saving, without deleting and recreating accounts. Local renewal and OfficeAce account-server access preserve those checks too. Both QwenWork editions retain enterprise, organization and team fields; derived enterprise credentials cannot use the personal-device renewal path and need a complete enterprise credential update at expiry. They are not consumed or silently downgraded; international compatibility remains unverified.
- Qoder / QwenWork device IDs and the original QuWork device identity needed for redemption use separately authorized fixed-file readers. Device IDs are never modified. Qoder International check-in and campaigns use a verified official UMID component; missing permission or components are reported rather than bypassing eligibility or per-device limits.
- Kuku restores user-selected Cookie SQLite database imports. OfficeAce account-server access supports the account proxy while retaining exact official-domain and source checks.
- AutoClaw International restores the official Zai authorization branch; plugins cannot read environment variables themselves.
- WorkBuddy China checks official connectivity before its single chat submission and selects the official backup origin or validated address candidates when needed. It preserves the original proxy and TLS verification and does not replay a submitted chat.
- Raccoon parses standard streaming responses directly without a redundant protocol-conversion round trip. Tools, reasoning, usage, error detection, real completion markers and bounded streaming are retained. The matching host grants capped compute budgets from actual input and new upstream content, with separate fuel, stack, memory and timeout diagnostics. Implemented but unverified.

All additions are implemented but unverified. Compilation, packaging and signatures do not establish live-account acceptance.

| Platform | LLM requests | Token auto-refresh | Model list | Balance query | Check-in | Claims |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| WorkBuddy (China) | ✓ (chat / image generation / reference editing) | ✓ | ✓ | ✓ | ✓ | ✓ |
| WorkBuddy (Global) | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Raccoon | ✓ (chat / generation / reference editing) | ✓ (legacy credentials; IAM requires sign-in) | ✓ | ✓ | ✓ | ✓ (first-login rewards) |
| CatPaw | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| AutoClaw (domestic / international) | ✓ | ✓ | ✓ | ✓ | ✓ | Implemented; unverified (Login active / Weekly Perks, client & newbie tasks, code redemption) |
| Qoder | ✓ | ✓ | ✓ | ✓ | ✓ | Implemented; unverified (Sonus and other limited-time credit rewards) |
| QwenWork (China / International) | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Cline (Free / Pass) | ✓ | ✓ | ✓ | ✓ | — | — |
| Accio (international / domestic) | ✓ | ✓ | ✓ | ✓ | — | — |
| ZCode (domestic / international) | ✓ | ✗ | ✗ | ✓ | — | ✓ |
| CodeArts | ✓ | ✓ | ✓ | ✓ | Implemented; unverified (WASM daily benefits) | ✓ |
| Trae | ✓ | ✓ | ✓ | ✓ | ✓ | Not provided by this plugin |
| Loomy (iFlytek) | ✓ (chat / image generation / reference editing) | ✗ | ✓ | ✓ | ✓ | — |
| KukuAI (Baidu Wenku) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| MonkeyCode (Chaitin, domestic / international) | ✓ | ✗ | ✓ | Implemented; unverified (WASM; international currency unit unconfirmed) | Implemented; unverified (WASM) | — |
| Command Code | ✓ | ✗ | ✓ | — | — | — |
| Antigravity (Google, Gemini / Claude / GPT-OSS) | ✓ (chat / image generation / reference editing) | ✓ | ✓ | Implemented; unverified (WASM: group & per-model quotas, AI Credits) | — | — |
| OfficeAce (Huawei Cloud, OfficeClaw) | ✓ | ✓ | ✓ | ✓ | ✓ | Implemented; unverified (new-user rewards share daily settlement) |
| OrcaTerm (Tencent Cloud) | Moved to [Agent2WebProviders](https://github.com/ProvidersPlugin/Agent2WebProviders) (Web provider) | — | — | — | — | — |
| ModelScope (MoDa, domestic / international) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| AtomCode (AtomGit / GitCode) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| LobsterAI (NetEase Youdao, Youdao Lobster) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Doubao Work | Implemented; unverified (chat / image generation) | ✗ | Implemented; unverified | Implemented; unverified | Implemented; unverified | Implemented; unverified |
| Kiro | ✓ (chat / tools / thinking / image input) | ✓ | ✓ | ✓ | ✗ | — |
| AStudio (iFlytek Astron Studio) | ✓ (chat / tools / thinking / image input) | ✗ | ✓ | ✓ | ✓ (daily sign-in credits) | — |
| QuWork | Implemented; unverified (chat / tools / reasoning / image input) | Implemented; unverified (account session) | Implemented; public catalog inspected | Implemented; unverified | Implemented; unverified | Implemented; unverified (daily rewards / benefit redemption / monthly gift records) |
| Baidu DuMate (WASM only) | Implemented; unverified (chat / tools / image input / image generation) | Implemented; unverified (short-lived model token; expired cookies require sign-in) | Reference static list; online catalog unconfirmed | Implemented; unverified (credits and expiring packages) | Implemented; unverified | Implemented; unverified (daily sign-in credits, shared with check-in) |
| Codar (WASM only) | Restricted upstream (single-turn without tools may pass; multi-turn / tools commonly return 403; image input / generation implemented, unverified) | Implemented; unverified (imported accounts; local links read official credentials only) | Implemented; unverified (curated and full online catalogs) | Implemented; unverified (general / Agent / rewrite points) | Implemented; unverified (rolling 24 hours) | Implemented; unverified (free-model activities) |

> The standalone Codar development package is in `wasm/codar`, based on [codar2api](https://github.com/AiXiaoc/codar2api) and [magpie-plugin-codar](https://github.com/MisonL/magpie-plugin-codar). It provides WeChat QR, SMS (including phone binding and new-IP verification), access-token and single-account JSON imports, and explicitly authorized local linking. The host renders the official Alicom4 CAPTCHA. Local links read the current official login without storing token snapshots or modifying client files. Automatic free-model claims are disabled by default. Upstream now restricts login credentials to the official client: only minimal single-turn requests without tools (and with sufficient `max_tokens`) may pass (panel single-turn tests do not indicate multi-turn or tool availability), whereas multi-turn history or tool definitions typically return `403 client_not_supported` (or `403 account_restricted` on some accounts). This plugin does not spoof official identity prompts to bypass the check and does not support real Agent client scenarios. No live-account validation was performed.

> The standalone DuMate development package is in `wasm/dumate`, based on the web-credential and cloud protocols in [dumate2api](https://github.com/wearetheone777/dumate2api); it is not part of the legacy native bundle. Cookie or single-account JSON imports validate identity online. App models and `qianfan/` mode share the same account and credits; unknown model names are never silently replaced. The host does not yet have a generic Baidu cookie-capture flow, so imports are manual. Check-in queries status before claiming and never replays uncertain results; lotteries and growth tasks are not automated. No live-account tests were run.

Custom providers remain a gateway feature and do not require this plugin. They support model requests and model-list fetching; other capabilities depend on the service you configure. API account creation skips a second proxy save only for `proxySaved: true`; older gateways retain a compatibility save and verification warning. Failure does not remove an added account or falsely report that its proxy was saved.

> Note: model-list markers cover online fetching or static catalogs as specified by each row and implementation. Missing refresh or usage hooks do not establish that the upstream lacks an endpoint. Qoder check-in supports both regions; the international flow requires the local UMID component. Limited-time credit rewards, including Sonus, are queried and claimed individually under Check-in Center → Event rewards, separately from daily check-in (implemented, not yet verified with a live claim); QwenWork has **no** check-in endpoint on either edition (nor a personal access token or claimable campaigns — those three genuinely do not exist upstream, rather than being "not implemented yet").
>
> QwenWork sign-in: the **China edition** uses the official OAuth page (authorization code with a loopback callback, the same route the official CLI / IDE takes) and the account joins the list by itself. The **international edition is hidden from the list** (2026-11-03). Its chat and usage paths work, but sign-in is blocked upstream: the upstream accepts only its own registered callback address, so the code never reaches this machine, and its device-code endpoint has been retired — an account simply cannot be added. The code stays in place; removing the id from the gateway's suspended list (`availability.rs`) re-enables it.
>
> Kiro: browser sign-in supports automatic local callbacks and full callback URL paste for remote use (update both the gateway and plugin). Google / GitHub use the Kiro portal; AWS Builder ID / IAM Identity Center can authorize directly to avoid an extra callback. The upstream has no daily check-in and no claimable campaigns.
> AStudio: model calls use OpenAI Responses; balance and daily sign-in credits require the account to carry a web session. Two sign-in routes: phone code or account/password on the in-app local sign-in page (sending a code and password sign-in require the GeeTest slider; the gateway and the browser must share one machine), or paste a `modelBearerToken` (fill `token` and `accountId` too for balance and check-in). Credentials can only be re-logged-in or re-pasted — the upstream has no refresh endpoint.

> QuWork: local import stores a read-only reference and never changes official client files; remote deployments can paste a credential JSON. `llmToken` is for model calls and `accessToken` is for account renewal, credits and rewards. They are not interchangeable. Model-only credentials cannot query balances or claim rewards. Daily claims check status first and never resubmit uncertain results. Monthly gifts are issued automatically upstream and are read-only here. Benefit redemption requires a user-supplied code or order number and confirmation; code activation needs the original client's device identity. Implemented against QuWork 1.6.0 and its public catalog, without live-account validation.

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
