use agent2api_provider_protocol as proto;

use crate::provider::BoxProvider;

#[derive(Clone, Copy, Default)]
pub struct ServeOptions {
    /// 在握手应答前初始化机器级资源；失败时拒绝握手。
    pub on_init: Option<fn(&proto::HostEnv) -> std::io::Result<()>>,
}

pub fn serve_blocking(providers: Vec<BoxProvider>) {
    serve_blocking_with(providers, ServeOptions::default());
}

pub fn serve_blocking_with(providers: Vec<BoxProvider>, options: ServeOptions) {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("provider host: 建 tokio 运行时失败: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = runtime.block_on(crate::serve::serve_with(providers, options)) {
        eprintln!("provider host: 主循环异常退出: {e}");
        std::process::exit(1);
    }
}
