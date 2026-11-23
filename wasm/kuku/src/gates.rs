//! 数据库文件由宿主只读解析本次用户选中的材料；不接收任意路径。
//! 遇到 encrypted_value 不自行调用系统解密，也不执行客户端进程。

use agent2api_wasm_sdk::{json, Error, Value};

pub fn desktop_error() -> Error {
    Error::http(
        400,
        "库库本机导入请通过添加账号页面的导入本机登录态入口提交，不使用网页登录等待流程",
    )
    .with_code("kuku_cookie_source_unavailable")
}
pub fn requirements() -> Value {
    json!({ "implemented": true, "browser": { "implemented": true, "desktopOnly": true,
        "startUrl": "https://kuku.baidu.com/", "visible": true, "flowIsolated": true,
        "navigationHosts": ["kuku.baidu.com", "passport.baidu.com", "wappass.baidu.com", "baidu.com"],
        "source": "仅本次provider+flow独立浏览器会话，含HttpOnly；禁止读取系统浏览器或其他任务Cookie",
        "projection": ["BDUSS", "BDUSS_BFESS", "STOKEN", "PTOKEN", "PTOKEN_BFESS", "BAIDUID", "BAIDUID_BFESS", "PANPSC", "BIDUPSID", "PSTM", "PASS_STOKEN"],
        "lifecycle": "一次性提交，完成/取消/过期清理；不得把Cookie放URL查询串或OAuth授权码" },
        "sqlite": { "implemented": true, "desktopPlaintext": true,
            "input": ["用户本次选定的sealed资源ID", "用户明确导入的固定客户端来源"],
            "fixedWindowsSources": ["%APPDATA%/baidugenflowpro/Network/Cookies", "%LOCALAPPDATA%/baidugenflowpro/Network/Cookies"],
            "table": "cookies", "columns": ["host_key", "name", "value", "path", "encrypted_value"],
            "domains": "baidu.com及其真实子域边界；仅投影协议必需Cookie，不是LIKE %baidu%",
            "output": "受限Cookie行(name,value,domain,path)+明确source，不返回文件路径、SQL或整库",
            "restrictions": "只读、参数化固定查询、大小/行数限制；无ATTACH/扩展/系统解密/任意路径；临时资源成功失败均清理" } })
}
