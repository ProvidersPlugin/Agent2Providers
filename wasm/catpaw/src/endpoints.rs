//! 插件直接业务请求仍仅目录与积分；不接受用户覆盖host/path。
//! 网页登录三条固定请求由宿主catpaw.passport.v1执行，插件不自行请求登录端点。

pub const PROVIDER: &str = "catpaw";
pub const DESKTOP_ID: &str = "desktop-auth";
pub const MODELS_URL: &str = "https://ai.catpaw.meituan.com/api/agent/maas/model-types";
pub const BALANCE_URL: &str = "https://catx.nocode.cn/api/gateway/credit/balance";
pub const APP_KEY: &str = "fe_com.sankuai.catpaw.external.front";
pub const CLIENT_VERSION: &str = "1.0.1";
pub const MAX_TOKEN_CHARS: usize = 8192;
pub const MAX_USER_CHARS: usize = 256;
pub const MAX_INPUT_BYTES: usize = 128 * 1024;
