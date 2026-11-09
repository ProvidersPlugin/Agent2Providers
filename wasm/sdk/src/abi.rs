use crate::{json, Error, Result, Value};

const MAX_MESSAGE: usize = 16 * 1024 * 1024;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "agent2api")]
extern "C" {
    #[link_name = "call"]
    fn host_call(pointer: i32, length: i32) -> i32;
    #[link_name = "read"]
    fn host_read(pointer: i32, capacity: i32) -> i32;
}

pub fn allocate(length: i32) -> i32 {
    if length < 0 || length as usize > MAX_MESSAGE { return 0; }
    let buffer = vec![0_u8; length as usize].into_boxed_slice();
    Box::into_raw(buffer) as *mut u8 as i32
}

// 输入只允许来自 a2a_alloc；宿主每次调用后销毁实例，未释放的输出随实例回收。
pub unsafe fn dispatch(pointer: i32, length: i32, handler: fn(&str, Value) -> Result<Value>) -> i64 {
    let result = if pointer <= 0 || length < 0 || length as usize > MAX_MESSAGE {
        Err(Error::new("插件输入长度无效"))
    } else {
        let buffer = Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer as *mut u8, length as usize));
        let input = serde_json::from_slice::<Value>(&buffer).map_err(|_| Error::new("插件输入不是有效 JSON"));
        input.and_then(|input| handler(input["method"].as_str().unwrap_or(""), input["params"].clone()))
    };
    let reply = match result { Ok(result) => json!({ "result": result }), Err(error) => json!({ "error": error }) };
    let mut bytes = serde_json::to_vec(&reply).unwrap_or_default();
    if bytes.len() > MAX_MESSAGE {
        bytes = b"{\"error\":{\"message\":\"Plugin response exceeds limit\"}}".to_vec();
    }
    let length = bytes.len() as u64;
    let pointer = Box::into_raw(bytes.into_boxed_slice()) as *mut u8 as u32;
    ((length << 32) | pointer as u64) as i64
}

pub fn host(method: &str, params: Value) -> Result<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    { let _ = (method, params); Err(Error::new("插件只能由 Wasm 宿主调用")) }
    #[cfg(target_arch = "wasm32")]
    {
        let bytes = serde_json::to_vec(&json!({ "method": method, "params": params }))
            .map_err(|_| Error::new("插件请求无法编码"))?;
        if bytes.len() > MAX_MESSAGE { return Err(Error::new("插件请求过大")); }
        let length = unsafe { host_call(bytes.as_ptr() as i32, bytes.len() as i32) };
        if length < 0 || length as usize > MAX_MESSAGE { return Err(Error::new("主程序返回长度无效")); }
        let mut buffer = vec![0; length as usize];
        if unsafe { host_read(buffer.as_mut_ptr() as i32, length) } != length {
            return Err(Error::new("主程序返回内容不完整"));
        }
        let result: Value = serde_json::from_slice(&buffer).map_err(|_| Error::new("主程序返回内容无效"))?;
        if let Some(error) = result.get("error") {
            return Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| Error::new("主程序调用失败")));
        }
        result.get("result").cloned().ok_or_else(|| Error::new("主程序缺少调用结果"))
    }
}
