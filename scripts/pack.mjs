#!/usr/bin/env node
/**
 * 插件包打包脚本：把一个平台的 release 二进制 + pack.json 打成可直接安装的 zip。
 *
 * ── 为什么脚本在插件仓库里（而不是网关仓库的 scripts/）─────────
 * 插件源码将来整体搬到独立仓库（计划 P7），CI 也在那边跑。打包是插件自己的
 * 构建步骤，必须跟着源码走 —— 放在网关仓库会让「独立仓库 checkout 后没有
 * 打包脚本」变成一个必然踩到的坑。
 *
 * ── 产物形状（与网关 `plugin_host::install` 的期望逐字对应）──────
 * ```text
 * agent2api-providers-<version>-<target>.zip
 *   pack.json                     ← 原样拷自 pack.json
 *   bin/agent2api-providers[.exe] ← release 二进制
 * ```
 * 网关把 zip 解到 `plugins/<id>/<version>/`，读 `pack.json` 的 `entry` 找二进制，
 * 所以**目录结构不能改**（`bin/` 那一层是 `pack.json` 里写死的）。
 *
 * ── 一次只打一个平台 ────────────────────────────────────────
 * 脚本不负责交叉编译（那要 mingw / osxcross / musl 工具链，塞进来只会让
 * 「在 macOS 上打 Windows 包」变成一个必然失败的分支）。它只做
 * 「已经编好的二进制 → zip」；多平台由 CI 矩阵各跑一次
 * （见 `.github/workflows/build.yml`），每个平台上传自己的 artifact。
 *
 * ── 用法 ────────────────────────────────────────────────────
 * ```bash
 * node scripts/pack.mjs                       # 打当前平台（顺带 cargo build）
 * node scripts/pack.mjs --target <三元组>      # 打指定平台（需已交叉编好）
 * node scripts/pack.mjs --skip-build          # 不重编，直接打现有二进制
 * node scripts/pack.mjs --out /tmp            # 指定输出目录
 * ```
 *
 * 退出码：0 = 成功；1 = 失败（二进制不存在 / pack.json 与目标平台对不上）。
 */

import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import process from 'node:process'

const HERE = dirname(fileURLToPath(import.meta.url))
/** 插件仓库根（本脚本在 `<根>/scripts/` 下） */
const PLUGIN_DIR = resolve(HERE, '..')

/** 宿主平台的三元组（与网关 `plugin_host::rpc::target_triple()` 同一口径：
 *  完整的 Rust 三元组，例如 `x86_64-pc-windows-msvc`）。
 *
 *  ⚠ 这个字符串有四个消费方，必须逐字一致：`HostEnv.target`、`pack.json` 的
 *  `entry` 键、签名载荷里的 `target=`、更新资产名 `…-<target>.zip`。签名是
 *  逐字比对，写成 `x86_64-windows` 会让「签好的包验不过」而错误信息只说
 *  「签名与本包内容不匹配」。 */
function hostTriple() {
  const arch = process.arch === 'x64' ? 'x86_64' : process.arch === 'arm64' ? 'aarch64' : process.arch
  switch (process.platform) {
    case 'win32': return `${arch}-pc-windows-msvc`
    case 'darwin': return `${arch}-apple-darwin`
    default: return `${arch}-unknown-linux-gnu`
  }
}

/** 目标平台的入口二进制名（Windows 带 .exe）。
 *
 *  按 **target** 判断而不是宿主：交叉编译时两者不同（在 Linux 上打 Windows 包
 *  会去找 `.exe`，而宿主的 `process.platform` 给不出这个信息）。 */
function binaryName(target) {
  return target.includes('windows') ? 'agent2api-providers.exe' : 'agent2api-providers'
}

/**
 * 收集清单 `icons` 目录下的全部文件，返回 `[{ name, data, executable }]`。
 *
 * ── 为什么这里要校验路径 ────────────────────────────────────
 * `icons` 是包内相对路径（与 `entry` 同一口径）。它若写成绝对路径或带 `..`，
 * 打包时就会把开发机上的任意文件塞进包里，装到用户那边变成「包里多了一个
 * 不该有的文件」。所以与网关 `validate_manifest` 同一套判据：相对、不含 `..`。
 *
 * 目录不存在时**不报错**：`icons` 是可选字段，写了但目录还没建只说明这一版
 * 没带图标，界面回落字母即可 —— 为一张图标让整个打包失败不值得。
 */
function collectDir(field, dir) {
  if (!dir) return []
  if (dir.startsWith('/') || dir.startsWith('\\') || /^[a-zA-Z]:/.test(dir)) {
    throw new Error(`pack.json 的 ${field} 必须是相对路径（${dir}）`)
  }
  if (dir.split(/[\\/]/).includes('..')) {
    throw new Error(`pack.json 的 ${field} 不能包含 ..（${dir}）`)
  }
  const root = join(PLUGIN_DIR, dir)
  if (!existsSync(root) || !statSync(root).isDirectory()) return []

  const files = []
  // 目录层级不深（assets/ 一层），但写成递归免得以后加子目录时静默漏文件
  const walk = current => {
    for (const item of readdirSync(current, { withFileTypes: true })) {
      const full = join(current, item.name)
      if (item.isDirectory()) {
        walk(full)
      } else if (item.isFile()) {
        // zip 条目名用正斜杠，且相对于包根（与 `icons` 字段的语义一致）
        const name = relative(PLUGIN_DIR, full).split('\\').join('/')
        files.push({ name, data: readFileSync(full), executable: false })
      }
    }
  }
  walk(root)
  return files.sort((a, b) => a.name.localeCompare(b.name))
}

/** 图标目录（`pack.json` 的 `icons` 字段）。 */
function collectIcons(manifest) {
  return collectDir('icons', manifest.icons)
}

/**
 * 收集 `pack.json` 的 `ui` 页面目录下的全部文件。
 *
 * ── entry 是相对 `ui/` 的，不是相对包根 ──────────────────────
 * 网关拼页面地址用的是 `/plugins/<pack>/ui/<entry>`（见 `pack_ui_pages`），
 * 而 `resolve_ui_file` 去掉 `/ui/` 前缀后再落到 `<包版本目录>/ui/` 下找 ——
 * 也就是说 `entry` 的基准目录是 `ui/`。所以这里收集时要把 `ui/` 前缀补回去，
 * 否则 `collectDir` 会去仓库根找 `checkin/`，一个文件都收不到（打包不报错，
 * 装出来的包却打不开页面）。
 *
 * ── 为什么 ui 也要校验路径 ──────────────────────────────────
 * 与 `icons` 同一理由：包内相对路径，写错了会把开发机上的任意文件打进包。
 * 额外的判据来自网关侧：`ui` 页面的 `entry` 必须以 `.html` 结尾、不含 `..` ——
 * 这里同步挡一遍，让「打出来的包装不上」提前到打包时暴露。
 */
function collectUi(manifest) {
  const pages = Array.isArray(manifest.ui) ? manifest.ui : []
  const files = []
  const seen = new Set()
  for (const page of pages) {
    const entry = String(page.entry || '').replace(/\\/g, '/')
    if (!entry.toLowerCase().endsWith('.html')) {
      throw new Error(`pack.json 的 ui 页面 ${page.id} 的 entry 必须以 .html 结尾（${entry}）`)
    }
    if (entry.split('/').includes('..') || entry.startsWith('/')) {
      throw new Error(`pack.json 的 ui 页面 ${page.id} 的 entry 不能是绝对路径或含 ..（${entry}）`)
    }
    // 页面资源目录 = entry 所在的目录（约定：一个页面一个目录），基准是 ui/
    const dir = entry.split('/').slice(0, -1).join('/')
    if (!dir) throw new Error(`pack.json 的 ui 页面 ${page.id} 的 entry 必须在子目录里（${entry}）`)
    if (seen.has(dir)) continue
    seen.add(dir)
    files.push(...collectDir('ui', `ui/${dir}`))
  }
  const documentation = join(PLUGIN_DIR, 'ui', 'provider-docs.json')
  if (existsSync(documentation) && statSync(documentation).isFile()) {
    files.push({ name: 'ui/provider-docs.json', data: readFileSync(documentation), executable: false })
  }
  return files
}

function parseArgs(argv) {
  const options = { out: join(PLUGIN_DIR, 'dist'), build: true, target: '' }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    if (arg === '--out') options.out = resolve(argv[++index] || options.out)
    else if (arg === '--target') options.target = argv[++index] || ''
    else if (arg === '--skip-build') options.build = false
    else if (arg === '--help' || arg === '-h') {
      console.log('用法: node scripts/pack.mjs [--target <三元组>] [--out <目录>] [--skip-build]')
      process.exit(0)
    } else {
      console.error(`未知参数：${arg}`)
      process.exit(1)
    }
  }
  return options
}

/**
 * 最小 zip 写入器（store 模式，不压缩）。
 *
 * ── 为什么不用现成的 zip 库 ─────────────────────────────────
 * 这个脚本要在**任何**开发机与 CI runner 上直接跑，而仓库没有 node_modules。
 * 为一个 12MB 的二进制引入 archiver / jszip 会让「打包插件」这件事多一次
 * `npm install`。store 模式（不压缩）写 zip 只需要 CRC32 + 两份目录，
 * 六十行就能写完；体积代价是 zip 与原始二进制一样大（约 12MB）—— 对一次
 * 手动分发完全可接受。
 *
 * 网关侧用 `zip` crate 解包，它认标准 store 条目，不需要额外处理。
 */
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

/** DOS 时间戳（zip 的 local header 要它；用固定值避免同一份输入产出不同字节） */
const DOS_TIME = 0
const DOS_DATE = ((2026 - 1980) << 9) | (1 << 5) | 1

/**
 * 打一个 store 模式的 zip。
 * `entries` 是 `[{ name, data, executable }]`，name 用正斜杠（zip 规范）。
 */
function writeZip(entries) {
  const locals = []
  const centrals = []
  let offset = 0
  for (const entry of entries) {
    const nameBytes = Buffer.from(entry.name, 'utf8')
    const crc = crc32(entry.data)
    const size = entry.data.length

    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0) // 签名
    local.writeUInt16LE(20, 4) // 解压所需版本
    local.writeUInt16LE(0x0800, 6) // 通用标志：UTF-8 文件名
    local.writeUInt16LE(0, 8) // 压缩方法 0 = store
    local.writeUInt16LE(DOS_TIME, 10)
    local.writeUInt16LE(DOS_DATE, 12)
    local.writeUInt32LE(crc, 14)
    local.writeUInt32LE(size, 18) // 压缩后大小
    local.writeUInt32LE(size, 22) // 原始大小
    local.writeUInt16LE(nameBytes.length, 26)
    local.writeUInt16LE(0, 28) // extra 长度
    locals.push(local, nameBytes, entry.data)

    const central = Buffer.alloc(46)
    central.writeUInt32LE(0x02014b50, 0)
    central.writeUInt16LE((3 << 8) | 20, 4) // Unix 属性，ZIP 2.0
    central.writeUInt16LE(20, 6) // 解压所需版本
    central.writeUInt16LE(0x0800, 8)
    central.writeUInt16LE(0, 10)
    central.writeUInt16LE(DOS_TIME, 12)
    central.writeUInt16LE(DOS_DATE, 14)
    central.writeUInt32LE(crc, 16)
    central.writeUInt32LE(size, 20)
    central.writeUInt32LE(size, 24)
    central.writeUInt16LE(nameBytes.length, 28)
    central.writeUInt16LE(0, 30) // extra
    central.writeUInt16LE(0, 32) // comment
    central.writeUInt16LE(0, 34) // disk
    central.writeUInt16LE(0, 36) // internal attrs
    // 外部属性：高 16 位是 Unix mode。二进制给 0755，清单给 0644 ——
    // 少了这一项，Linux/macOS 上解出来的二进制没有可执行位（网关 spawn 会
    // 报 EACCES，而错误信息指向「插件启动失败」，很难反推到权限）
    const mode = entry.executable ? 0o100755 : 0o100644
    central.writeUInt32LE((mode << 16) >>> 0, 38)
    central.writeUInt32LE(offset, 42)
    centrals.push(central, nameBytes)

    offset += local.length + nameBytes.length + size
  }
  const centralSize = centrals.reduce((total, item) => total + item.length, 0)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(0, 4) // disk
  end.writeUInt16LE(0, 6) // 起始 disk
  end.writeUInt16LE(entries.length, 8)
  end.writeUInt16LE(entries.length, 10)
  end.writeUInt32LE(centralSize, 12)
  end.writeUInt32LE(offset, 16)
  end.writeUInt16LE(0, 20) // comment 长度
  return Buffer.concat([...locals, ...centrals, end])
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  const host = hostTriple()
  const target = options.target || host

  // ── 先校验清单（便宜），再编 / 找二进制（贵）─────────────────
  // 顺序反过来的话，`--target` 写错时用户看到的是「找不到 release 二进制」，
  // 会以为是没编过，实际上是这个平台压根没在 pack.json 里声明。
  const manifestPath = join(PLUGIN_DIR, 'pack.json')
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
  const declared = manifest.entry?.[target]
  if (!declared) {
    console.error(`[pack] pack.json 的 entry 里没有目标平台（${target}）`)
    console.error(`[pack] 现有条目：${Object.keys(manifest.entry || {}).join(', ') || '(空)'}`)
    process.exit(1)
  }
  const expectedName = binaryName(target)
  if (declared.split('/').pop() !== expectedName) {
    console.error(`[pack] pack.json 的 entry（${declared}）与二进制名（${expectedName}）不一致`)
    process.exit(1)
  }

  // 目标 == 宿主 → cargo 原生构建，产物在 target/release/；
  // 目标 != 宿主 → 传 --target，产物在 target/<triple>/release/。
  // 路径必须按这个规则取：交叉编译时 target/release/ 里是宿主架构的旧产物，
  // 拿错会把宿主的二进制打进别的平台的包里（能装、能验签、跑起来 exec format error）。
  // 独占最终构建可使用新的 CARGO_TARGET_DIR；打包必须取同一目录的真实产物，
  // 不能退回插件默认 target/ 读取旧二进制。相对目录与 cargo cwd 保持一致。
  const targetDir = process.env.CARGO_TARGET_DIR
    ? resolve(PLUGIN_DIR, process.env.CARGO_TARGET_DIR)
    : join(PLUGIN_DIR, 'target')
  const native = target === host
  const exe = native
    ? join(targetDir, 'release', binaryName(target))
    : join(targetDir, target, 'release', binaryName(target))

  if (options.build) {
    const args = ['build', '--release', '-p', 'agent2api-providers']
    if (!native) args.push('--target', target)
    console.log(`[pack] 编译 release 产物（cargo ${args.join(' ')}）…`)
    execFileSync('cargo', args, { cwd: PLUGIN_DIR, stdio: 'inherit' })
  }
  if (!existsSync(exe)) {
    console.error(`[pack] 找不到 release 二进制：${exe}`)
    console.error(`[pack] 先跑一次 \`cd ${PLUGIN_DIR} && cargo build --release${native ? '' : ` --target ${target}`}\`，或去掉 --skip-build`)
    process.exit(1)
  }

  const zipName = `${manifest.id}-${manifest.version}-${target}.zip`
  const zipPath = join(options.out, zipName)
  mkdirSync(options.out, { recursive: true })

  const binary = readFileSync(exe)
  const icons = collectIcons(manifest)
  const ui = collectUi(manifest)
  const zip = writeZip([
    { name: 'pack.json', data: Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`, 'utf8'), executable: false },
    ...icons,
    ...ui,
    ...collectDir('licenses', 'licenses'),
    { name: declared, data: binary, executable: true },
  ])
  writeFileSync(zipPath, zip)

  const binaryMb = (statSync(exe).size / 1024 / 1024).toFixed(2)
  const zipMb = (zip.length / 1024 / 1024).toFixed(2)
  console.log(`[pack] 平台      ${target}${native ? '（宿主）' : '（交叉）'}`)
  console.log(`[pack] 二进制    ${binaryMb} MB`)
  console.log(`[pack] 图标      ${icons.length ? `${icons.length} 个（${manifest.icons}/）` : '无'}`)
  const pageCount = Array.isArray(manifest.ui) ? manifest.ui.length : 0
  console.log(`[pack] 页面      ${pageCount ? `${pageCount} 个（${ui.length} 个文件）` : '无'}`)
  console.log(`[pack] 插件包    ${zipPath}`)
  console.log(`[pack] 体积      ${zipMb} MB（store 模式，未压缩）`)
  console.log('[pack] 安装方式：网关界面 → 提供商插件 → 安装插件包，选中这个 zip')
}

main()
