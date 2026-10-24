#!/usr/bin/env node
/**
 * 插件包签名密钥的生成与签名。
 *
 * ── 为什么脚本在插件仓库里（而不是网关仓库的 scripts/）─────────
 * 签名是**插件发布流程**的一步，与打包（`scripts/pack.mjs`）同一层。放在
 * 网关仓库会让「独立仓库 checkout 后没有签名脚本」变成一个必然踩到的坑。
 *
 * ── 两个子命令 ──────────────────────────────────────────────
 * ```bash
 * # 1. 生成一对密钥（只需一次；私钥务必离线保管）
 * node scripts/sign.mjs keygen
 *
 * # 2. 给已打好的 zip 签名（产出写回原 zip，并打印签名块）
 * node scripts/sign.mjs sign --zip dist/agent2api-providers-0.1.0-x86_64-pc-windows-msvc.zip
 * ```
 *
 * ── 为什么签名在**打好的 zip 之外**做 ────────────────────────
 * 签名覆盖的是包内容（见 `agent2api-provider-protocol::signing_payload`），
 * 而 zip 只是内容的容器。所以流程是「先打包 → 再按包里的内容签名 → 把签名
 * 写回 `pack.json` → 重新打包」，而不是「签 zip 字节」。`sign` 子命令把这三步
 * 串起来：它读 zip 里的 `pack.json` 与入口二进制、算出载荷、签名、写回
 * `pack.json`、重新压一次 zip（覆盖原文件）。
 *
 * ── 私钥从哪来 ──────────────────────────────────────────────
 * `--key <path>` 指定，缺省读 `~/.agent2api/plugin-signing-key.json`。
 * 环境变量 `AGENT2API_PLUGIN_KEY` 也可给（CI 用 secret 注入）。私钥**绝不**
 * 进仓库：`.gitignore` 已排除 `*.key.json`。
 *
 * ── 依赖 ────────────────────────────────────────────────────
 * Node 内置的 `crypto` 支持 ed25519（`generateKeyPairSync('ed25519')` /
 * `sign(null, data, privateKey)`），所以不需要任何 npm 依赖 —— 与
 * `pack.mjs` 同一取向（仓库没有 node_modules）。
 */

import { createHash, generateKeyPairSync, sign as cryptoSign } from 'node:crypto'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const HERE = dirname(fileURLToPath(import.meta.url))
/** 插件仓库根（本脚本在 `<根>/scripts/` 下） */
const PLUGIN_DIR = resolve(HERE, '..')

/* ─── 最小 zip 读写（store 模式；与 pack.mjs 同一套）───────────── */

function crc32(buffer) {
  let table = crc32.table
  if (!table) {
    table = new Uint32Array(256)
    for (let index = 0; index < 256; index += 1) {
      let value = index
      for (let bit = 0; bit < 8; bit += 1) {
        value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1
      }
      table[index] = value >>> 0
    }
    crc32.table = table
  }
  let crc = 0xffffffff
  for (let index = 0; index < buffer.length; index += 1) {
    crc = table[(crc ^ buffer[index]) & 0xff] ^ (crc >>> 8)
  }
  return (crc ^ 0xffffffff) >>> 0
}

const DOS_TIME = 0
const DOS_DATE = ((2026 - 1980) << 9) | (1 << 5) | 1

function writeZip(entries) {
  const locals = []
  const centrals = []
  let offset = 0
  for (const entry of entries) {
    const nameBytes = Buffer.from(entry.name, 'utf8')
    const crc = crc32(entry.data)
    const size = entry.data.length
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt16LE(0x0800, 6)
    local.writeUInt16LE(0, 8)
    local.writeUInt16LE(DOS_TIME, 10)
    local.writeUInt16LE(DOS_DATE, 12)
    local.writeUInt32LE(crc, 14)
    local.writeUInt32LE(size, 18)
    local.writeUInt32LE(size, 22)
    local.writeUInt16LE(nameBytes.length, 26)
    local.writeUInt16LE(0, 28)
    locals.push(local, nameBytes, entry.data)

    const central = Buffer.alloc(46)
    central.writeUInt32LE(0x02014b50, 0)
    central.writeUInt16LE((3 << 8) | 20, 4)
    central.writeUInt16LE(20, 6)
    central.writeUInt16LE(0x0800, 8)
    central.writeUInt16LE(0, 10)
    central.writeUInt16LE(DOS_TIME, 12)
    central.writeUInt16LE(DOS_DATE, 14)
    central.writeUInt32LE(crc, 16)
    central.writeUInt32LE(size, 20)
    central.writeUInt32LE(size, 24)
    central.writeUInt16LE(nameBytes.length, 28)
    central.writeUInt16LE(0, 30)
    central.writeUInt16LE(0, 32)
    central.writeUInt16LE(0, 34)
    central.writeUInt16LE(0, 36)
    const mode = entry.executable ? 0o100755 : 0o100644
    central.writeUInt32LE((mode << 16) >>> 0, 38)
    central.writeUInt32LE(offset, 42)
    centrals.push(central, nameBytes)
    offset += local.length + nameBytes.length + size
  }
  const centralSize = centrals.reduce((total, item) => total + item.length, 0)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(0, 4)
  end.writeUInt16LE(0, 6)
  end.writeUInt16LE(entries.length, 8)
  end.writeUInt16LE(entries.length, 10)
  end.writeUInt32LE(centralSize, 12)
  end.writeUInt32LE(offset, 16)
  end.writeUInt16LE(0, 20)
  return Buffer.concat([...locals, ...centrals, end])
}

/**
 * 读 zip（只支持 store 模式 —— 我们自己的打包脚本产出的就是这种）。
 *
 * 不支持 deflate 是刻意的：签名要读**内容**，而一个用别家工具打的 deflate 包
 * 需要解压库。真遇到那种包时，正确做法是用 `scripts/pack.mjs` 重新打一次。
 */
function readZip(buffer) {
  const entries = []
  let offset = 0
  while (offset + 30 <= buffer.length && buffer.readUInt32LE(offset) === 0x04034b50) {
    const method = buffer.readUInt16LE(offset + 8)
    if (method !== 0) {
      throw new Error(`zip 条目使用了压缩（method=${method}），请用 scripts/pack.mjs 重新打包`)
    }
    const size = buffer.readUInt32LE(offset + 18)
    const nameLength = buffer.readUInt16LE(offset + 26)
    const extraLength = buffer.readUInt16LE(offset + 28)
    const name = buffer.toString('utf8', offset + 30, offset + 30 + nameLength)
    const dataStart = offset + 30 + nameLength + extraLength
    entries.push({ name, data: buffer.subarray(dataStart, dataStart + size) })
    offset = dataStart + size
  }
  if (!entries.length) throw new Error('读不到任何 zip 条目（不是 store 模式的包？）')
  return entries
}

/* ─── 签名载荷（必须与 Rust 侧 `signing_payload` 逐字一致）────── */

/**
 * 与 `agent2api-provider-protocol::provider::signing_payload` 同一份拼装规则。
 *
 * ⚠ 两处必须逐字一致：改任何一行都要同步 Rust 那边，否则「所有包都验不过」，
 * 而失败信息只会说「签名与本包内容不匹配」，看不出是拼装规则写岔了。
 */
function signingPayload(manifest, target, binarySha256, uiFiles) {
  const providers = manifest.providers
    .map(item => `${item.id}:${item.order ?? 0}:${item.default_enabled === false ? 0 : 1}`)
    .sort()
  const entries = Object.entries(manifest.entry || {})
    .map(([key, value]) => `${key}=${value}`)
    .sort()
  // UI 资源清单：`相对包根的 POSIX 路径=sha256`，按路径字典序。
  // 没有 UI 资源时是空串 —— 老包的载荷因此逐字节不变。
  const ui = (uiFiles || [])
    .map(([path, hash]) => `${path}=${hash}`)
    .sort()
  return [
    'agent2api-pack-v1',
    `id=${manifest.id}`,
    `version=${manifest.version}`,
    `protocol=${manifest.protocol}`,
    `target=${target}`,
    `providers=${providers.join(',')}`,
    `entry=${entries.join(',')}`,
    `binary=${binarySha256}`,
    `ui=${ui.join(',')}`,
    '',
  ].join('\n')
}

/**
 * 收集包里的 UI 资源清单：`[[相对路径, sha256], …]`。
 *
 * ── 为什么必须签 UI ────────────────────────────────────────
 * 插件页面跑在管理面板的 origin 下、能拿到面板令牌。只签二进制的话，
 * 一个包可以带合法签名的 exe **加**一个偷令牌的 js，验签照样通过。
 *
 * 判据与 Rust 侧 `plugin_host::install` 的 `collect_ui_files` 同源：
 * 只收 `ui/` 前缀下的条目（POSIX 路径，`\` 归一成 `/`），跳过目录条目
 * （zip 里的目录项以 `/` 结尾）。
 */
function collectUiFiles(entries) {
  return entries
    .filter(entry => !entry.name.endsWith('/') && entry.name.replace(/\\/g, '/').startsWith('ui/'))
    .map(entry => [
      entry.name.replace(/\\/g, '/'),
      createHash('sha256').update(entry.data).digest('hex'),
    ])
}

/* ─── 子命令 ───────────────────────────────────────────────── */

function keyPath(argv) {
  const index = argv.indexOf('--key')
  if (index >= 0 && argv[index + 1]) return resolve(argv[index + 1])
  const fromEnv = process.env.AGENT2API_PLUGIN_KEY
  if (fromEnv) return resolve(fromEnv)
  return join(homedir(), '.agent2api', 'plugin-signing-key.json')
}

function keygen(argv) {
  const path = keyPath(argv)
  if (existsSync(path)) {
    console.error(`[sign] 私钥已存在：${path}`)
    console.error('[sign] 换密钥会让所有已发出的包验不过 —— 确认要换就先备份再删掉它')
    process.exit(1)
  }
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const publicHex = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32).toString('hex')
  const privatePem = privateKey.export({ type: 'pkcs8', format: 'pem' })
  const keyId = `official-${new Date().getFullYear()}`
  writeFileSync(path, JSON.stringify({ keyId, privateKeyPem: privatePem }, null, 2), 'utf8')
  console.log(`[sign] 私钥已写入：${path}`)
  console.log('[sign] ⚠ 这个文件绝不能进仓库，也不要发给别人')
  console.log('')
  console.log('[sign] 把下面这段填进网关的 OFFICIAL_PUBKEYS')
  console.log('       （agent2api 仓库 server/src/server/core/plugin_host/install.rs）：')
  console.log('')
  console.log(`    ("${keyId}", "${publicHex}"),`)
  console.log('')
  console.log(`[sign] 公钥（十六进制，32 字节）：${publicHex}`)
}

function sign(argv) {
  const zipIndex = argv.indexOf('--zip')
  const zipPath = zipIndex >= 0 ? resolve(argv[zipIndex + 1] || '') : ''
  if (!zipPath || !existsSync(zipPath)) {
    console.error('用法: node scripts/sign.mjs sign --zip <包.zip> [--key <私钥.json>] [--target <三元组>]')
    process.exit(1)
  }
  const keyFile = keyPath(argv)
  if (!existsSync(keyFile)) {
    console.error(`[sign] 找不到私钥：${keyFile}`)
    console.error('[sign] 先跑一次：node scripts/sign.mjs keygen')
    process.exit(1)
  }
  const { keyId, privateKeyPem } = JSON.parse(readFileSync(keyFile, 'utf8'))

  const entries = readZip(readFileSync(zipPath))
  const manifestEntry = entries.find(entry => entry.name === 'pack.json')
  if (!manifestEntry) {
    console.error('[sign] 包里没有 pack.json')
    process.exit(1)
  }
  const manifest = JSON.parse(manifestEntry.data.toString('utf8'))

  // 目标平台：显式给的优先，否则从文件名解析（`...-<target>.zip`）
  let target = ''
  const targetIndex = argv.indexOf('--target')
  if (targetIndex >= 0 && argv[targetIndex + 1]) {
    target = argv[targetIndex + 1]
  } else {
    // 从 `<id>-<version>-<target>.zip` 里取 target。不能靠「最后一个 `-` 之后」
    // 切：三元组本身带连字符（x86_64-pc-windows-msvc 有两段）。
    // 更稳的做法是拿文件名里剩下的部分去和 pack.json 的 entry 键比对 ——
    // 那里就是权威清单，解析歧义直接消失。
    const base = basename(zipPath).replace(/\.zip$/, '')
    target = Object.keys(manifest.entry || {}).find(key => base.endsWith(`-${key}`)) || ''
  }
  if (!target || !manifest.entry?.[target]) {
    console.error(`[sign] 无法确定目标平台（解析结果：${target || '空'}）`)
    console.error(`[sign] pack.json 里的平台：${Object.keys(manifest.entry || {}).join(', ')}`)
    console.error('[sign] 用 --target <三元组> 显式指定')
    process.exit(1)
  }

  const binaryEntry = entries.find(entry => entry.name === manifest.entry[target])
  if (!binaryEntry) {
    console.error(`[sign] 包里没有 ${target} 的入口二进制：${manifest.entry[target]}`)
    process.exit(1)
  }
  const binarySha256 = createHash('sha256').update(binaryEntry.data).digest('hex')
  const uiFiles = collectUiFiles(entries)
  const payload = signingPayload(manifest, target, binarySha256, uiFiles)
  const signature = cryptoSign(null, Buffer.from(payload, 'utf8'), privateKeyPem)

  // 把签名写回 pack.json（其余条目原样保留）
  manifest.signature = {
    algorithm: 'ed25519',
    keyId,
    value: signature.toString('base64'),
    target,
  }
  const nextEntries = entries.map(entry =>
    entry.name === 'pack.json'
      ? { name: 'pack.json', data: Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, 'utf8') }
      : { name: entry.name, data: entry.data, executable: entry.name === manifest.entry[target] },
  )
  writeFileSync(zipPath, writeZip(nextEntries))

  console.log(`[sign] 已签名：${zipPath}`)
  console.log(`[sign] 平台    ${target}`)
  console.log(`[sign] 密钥    ${keyId}`)
  console.log(`[sign] 二进制  sha256=${binarySha256.slice(0, 16)}…`)
  console.log(`[sign] UI 资源 ${uiFiles.length} 个文件`)
  console.log(`[sign] 签名    ${signature.toString('base64').slice(0, 24)}…`)
  console.log('')
  console.log('[sign] 注意：签名覆盖的是**内容**。改了 pack.json 或二进制就要重签。')
}

const argv = process.argv.slice(2)
const command = argv[0]
if (command === 'keygen') keygen(argv)
else if (command === 'sign') sign(argv)
else {
  console.log('用法:')
  console.log('  node scripts/sign.mjs keygen [--key <私钥.json>]')
  console.log('  node scripts/sign.mjs sign --zip <包.zip> [--key <私钥.json>] [--target <三元组>]')
  process.exit(command ? 1 : 0)
}
