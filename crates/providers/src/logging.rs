//! 插件日志沿用调用方级别，调试详情由网关的调试开关决定是否入库。

#![allow(dead_code)]

use agent2api_provider_sdk::{log_debug, log_error, log_info, log_warn};

pub fn log(tag: &str, message: &str) {
    log_info!("{tag} {message}");
}

pub fn verbose(tag: &str, message: &str) {
    log_debug!("{tag} {message}");
}

pub fn warn(tag: &str, message: &str) {
    log_warn!("{tag} {message}");
}

pub fn error(tag: &str, message: &str) {
    log_error!("{tag} {message}");
}

pub fn now_ms() -> i64 {
    crate::http::now_ms()
}

// stdout 是 IPC 通道，控制台细节只能走日志回传。
pub fn console_line(tag: &str, message: &str) {
    log_debug!("{tag} {message}");
}
