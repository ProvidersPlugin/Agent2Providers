use std::path::Path;

use syn::visit::{self, Visit};

const STREAM_FUNCTIONS: &[(&str, &str)] = &[
    ("src/providers/codearts/adapter.rs", "conversation"),
    ("src/providers/trae/forward.rs", "send"),
    ("src/providers/accio/chat.rs", "send"),
    ("src/providers/qoder/chat.rs", "send"),
    ("src/providers/kuku/http.rs", "sse_post"),
    ("src/providers/orcaterm/chat.rs", "open_chat_stream"),
    ("src/providers/catpaw/upstream_http.rs", "turn_request"),
];

#[derive(Default)]
struct StreamCalls {
    violations: Vec<&'static str>,
}

impl<'ast> Visit<'ast> for StreamCalls {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            if path.path.segments.last().is_some_and(|segment| {
                segment.ident == "control_client" || segment.ident == "control_client_system_proxy"
            }) {
                self.violations.push("流式请求使用了控制面客户端");
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "timeout" {
            self.violations.push("流式请求设置了响应体总超时");
        }
        visit::visit_expr_method_call(self, call);
    }
}

struct FunctionGuard<'a> {
    name: &'a str,
    found: usize,
    violations: Vec<&'static str>,
}

impl FunctionGuard<'_> {
    fn inspect(&mut self, name: &syn::Ident, block: &syn::Block) {
        if name != self.name { return; }
        self.found += 1;
        let mut calls = StreamCalls::default();
        calls.visit_block(block);
        self.violations.extend(calls.violations);
    }
}

impl<'ast> Visit<'ast> for FunctionGuard<'_> {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.inspect(&function.sig.ident, &function.block);
        visit::visit_item_fn(self, function);
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        self.inspect(&function.sig.ident, &function.block);
        visit::visit_impl_item_fn(self, function);
    }
}

fn inspect(source: &str, function: &str) -> Result<Vec<&'static str>, String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let mut guard = FunctionGuard { name: function, found: 0, violations: Vec::new() };
    guard.visit_file(&file);
    if guard.found != 1 { return Err(format!("必须恰好找到一个 {function} 函数，实际 {}", guard.found)); }
    Ok(guard.violations)
}

fn run() -> Result<(), String> {
    // 构建闸门同时校验反例，避免扫描器失效后静默放行。
    for (source, rejected) in [
        ("fn send() { http::stream_client(None, \"direct\"); }", false),
        ("fn send() { let _ = \"control_client timeout\"; }", false),
        ("fn send() { http::control_client(None, \"direct\"); }", true),
        ("fn send() { http::control_client_system_proxy(); }", true),
        ("fn send() { stream_client().post(url).timeout(budget); }", true),
    ] {
        let rejected_by_guard = !inspect(source, "send")?.is_empty();
        if rejected_by_guard != rejected {
            return Err("流式客户端构建闸门自检失败".to_string());
        }
    }
    let root = std::env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?;
    for &(file, function) in STREAM_FUNCTIONS {
        println!("cargo:rerun-if-changed={file}");
        let source = std::fs::read_to_string(Path::new(&root).join(file))
            .map_err(|error| format!("读取 {file} 失败：{error}"))?;
        let violations = inspect(&source, function)?;
        if !violations.is_empty() {
            return Err(format!("{file}::{function}：{}", violations.join("；")));
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
