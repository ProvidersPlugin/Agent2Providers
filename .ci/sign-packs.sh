#!/usr/bin/env bash
# 给 dist/ 下的每个插件包签名。
#
# ── 为什么是独立脚本而不是复合 action ────────────────────────
# 这段逻辑原先住主仓库的 `.github/actions/sign-pack/`。复合 action 只能用
# **本仓库**的相对路径，而这里的工作流跨仓库 checkout 了源码 —— `uses:` 解析
# 的是构建仓库，`scripts/sign.mjs` 却在源码树里，路径对不上。改成脚本后
# 由 `shell: bash` 就地执行，源码树怎么写都不用管。
#
# ── 没配 secret 时不失败 ────────────────────────────────────
# 没配 `AGENT2API_PLUGIN_KEY` 时产物保持未签名（网关允许安装、界面标黄），
# 比让整个发版红掉更合适 —— 这与主仓库原 action 的取向一致。

set -euo pipefail

if [ -z "${SIGNING_KEY:-}" ]; then
  echo "::warning::没有 AGENT2API_PLUGIN_KEY，本次产物未签名"
  exit 0
fi

KEY_FILE="$RUNNER_TEMP/plugin-signing-key.json"
printf '%s' "$SIGNING_KEY" > "$KEY_FILE"
# runner 是临时的，但日志与残留文件仍不该留私钥
trap 'rm -f "$KEY_FILE"' EXIT

shopt -s nullglob
zips=(dist/*.zip)
if [ ${#zips[@]} -eq 0 ]; then
  echo "::error::dist/ 里没有 zip，打包步骤可能失败了"
  exit 1
fi

for zip in "${zips[@]}"; do
  node scripts/sign.mjs sign --zip "$zip" --key "$KEY_FILE"
done
