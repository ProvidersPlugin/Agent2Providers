//! 整次操作共享 30 秒保守宿主预算，单次 HTTP 使用剩余总期限。
//! 这是本包的资源边界，不是 MonkeyCode 官方服务时限承诺。

use crate::util;
use agent2api_wasm_sdk::{Error, Result};

pub const TIMEOUT_MS: u64 = 30_000;

pub struct Budget {
    started: i64,
    deadline: i64,
}

impl Budget {
    pub fn new() -> Result<Self> {
        let started = util::now()?;
        Ok(Self {
            started,
            deadline: started.saturating_add(TIMEOUT_MS as i64),
        })
    }

    pub fn remaining(&self) -> Result<u64> {
        let now = util::now()?;
        // 宿主 timeout_ms 下限为1000ms；余量不足1秒就不再开启连接，避免被宿主向上取整。
        if now < self.started || now.saturating_add(1000) > self.deadline {
            return Err(Error::http(
                504,
                "MonkeyCode 国际读取超过本包 30 秒总预算或时钟已变化，未继续请求",
            )
            .with_code("monkeycode_intl_deadline_exceeded"));
        }
        Ok((self.deadline - now) as u64)
    }
}
