//! 提供商注册表：把各家实现装进 `Vec<Box<dyn Provider>>`。
//!
//! ── 加一家新 provider 只改这里 ──────────────────────────────
//! 与网关原来的「加一家要改 6 个文件」不同：插件架构下网关不认识具体
//! provider，所以新增一家**只**需要：
//!   1. 在 `src/providers/<name>/` 写实现（实现 `Provider` trait）；
//!   2. 在本文件 `registry()` 里加一行；
//!   3. 在包的 `pack.json` 里 `providers[]` 加一条（界面显示用）。
//!
//! 网关侧零改动 —— 这正是这次重构的核心收益。
//!
//! ── 迁移进度 ────────────────────────────────────────────────
//! ```text
//!   已迁：demo（示例）、commandcode、modelscope / modelscope-intl
//!   待迁：其余 19 家（按 P3–P4 分批）
//! ```
//! 未迁的家继续由网关内置实现（`adapter_by_id` 先查内置枚举，命中就走内置）。

#[cfg(feature = "accio")]
pub mod accio;
#[cfg(feature = "codearts")]
pub mod codearts;
#[cfg(feature = "antigravity")]
pub mod antigravity;
#[cfg(feature = "atomcode")]
pub mod atomcode;
#[cfg(feature = "autoclaw")]
pub mod autoclaw;
#[cfg(feature = "catpaw")]
pub mod catpaw;
#[cfg(feature = "cline")]
pub mod cline;
#[cfg(feature = "commandcode")]
pub mod commandcode;
#[cfg(feature = "demo")]
pub mod demo;
#[cfg(feature = "kuku")]
pub mod kuku;
#[cfg(feature = "lobsterai")]
pub mod lobsterai;
#[cfg(feature = "loomy")]
pub mod loomy;
#[cfg(feature = "modelscope")]
pub mod modelscope;
#[cfg(feature = "monkeycode")]
pub mod monkeycode;
#[cfg(feature = "officeace")]
pub mod officeace;
#[cfg(feature = "orcaterm")]
pub mod orcaterm;
#[cfg(feature = "qoder")]
pub mod qoder;
#[cfg(feature = "raccoon")]
pub mod raccoon;
#[cfg(feature = "trae")]
pub mod trae;
#[cfg(feature = "workbuddy")]
pub mod workbuddy;
#[cfg(feature = "zcode")]
pub mod zcode;
#[cfg(feature = "doubaowork")]
pub mod doubaowork;

use agent2api_provider_sdk::BoxProvider;

/// 装出这个包提供的全部 provider。
///
/// ── 顺序有意义 ──────────────────────────────────────────────
/// 这个顺序就是 `init` 返回给网关的顺序，而网关的注册表按它排（同 `order`
/// 时）。加新家请加在**末尾** —— 插在中间会让存量用户的界面顺序变一下
/// （没有功能影响，但没必要动）。
pub fn registry() -> Vec<BoxProvider> {
    let mut list: Vec<BoxProvider> = Vec::new();

    #[cfg(feature = "cline")]
    {
        // Cline 的两个额度池是两个 provider、两个实例（同一份实现按池参数化）
        list.push(Box::new(cline::ClineProvider::FREE));
        list.push(Box::new(cline::ClineProvider::PASS));
    }

    #[cfg(feature = "demo")]
    list.push(Box::new(demo::DemoProvider));

    #[cfg(feature = "commandcode")]
    list.push(Box::new(commandcode::CommandCodeProvider));

    // ModelScope 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "modelscope")]
    {
        list.push(Box::new(modelscope::ModelScopeProvider::CN));
        list.push(Box::new(modelscope::ModelScopeProvider::INTL));
    }

    #[cfg(feature = "orcaterm")]
    list.push(Box::new(orcaterm::OrcaTermProvider));

    #[cfg(feature = "kuku")]
    list.push(Box::new(kuku::KukuProvider));

    // MonkeyCode 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "monkeycode")]
    {
        list.push(Box::new(monkeycode::MONKEYCODE_CN));
        list.push(Box::new(monkeycode::MONKEYCODE_INTL));
    }

    #[cfg(feature = "lobsterai")]
    list.push(Box::new(lobsterai::LobsterAiProvider));

    #[cfg(feature = "loomy")]
    list.push(Box::new(loomy::LoomyProvider));

    // Qoder 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "qoder")]
    {
        list.push(Box::new(qoder::QODER_CN));
        list.push(Box::new(qoder::QODER_INTL));
    }

    #[cfg(feature = "raccoon")]
    list.push(Box::new(raccoon::RaccoonProvider));

    #[cfg(feature = "atomcode")]
    list.push(Box::new(atomcode::AtomcodeProvider));

    // AutoClaw 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "autoclaw")]
    {
        list.push(Box::new(autoclaw::AutoClawProvider::CN));
        list.push(Box::new(autoclaw::AutoClawProvider::INTL));
    }

    #[cfg(feature = "officeace")]
    list.push(Box::new(officeace::OfficeAceProvider));

    // ZCode 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "zcode")]
    {
        list.push(Box::new(zcode::ZcodeProvider::CN));
        list.push(Box::new(zcode::ZcodeProvider::INTL));
    }

    // WorkBuddy 的两个地区同样是两个 provider、两个实例
    #[cfg(feature = "workbuddy")]
    {
        list.push(Box::new(workbuddy::WorkBuddyProvider::CN));
        list.push(Box::new(workbuddy::WorkBuddyProvider::INTL));
    }

    #[cfg(feature = "trae")]
    list.push(Box::new(trae::adapter::TraeProvider));

    #[cfg(feature = "catpaw")]
    list.push(Box::new(catpaw::adapter::CatPawProvider));

    // Accio 的两个地区是两个 provider、两个实例（同一份实现按地区参数化）
    #[cfg(feature = "accio")]
    {
        list.push(Box::new(accio::adapter::ACCIO));
        list.push(Box::new(accio::adapter::ACCIO_CN));
    }

    #[cfg(feature = "codearts")]
    list.push(Box::new(codearts::adapter::CODEARTS));
    #[cfg(feature = "antigravity")]
    list.push(Box::new(antigravity::adapter::ANTIGRAVITY));
    #[cfg(feature = "doubaowork")]
    list.push(Box::new(doubaowork::DoubaoWorkProvider));

    list
}
