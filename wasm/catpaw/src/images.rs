use agent2api_wasm_sdk::{json,Error,Result,Value};

pub fn block(value:&Value)->Result<Value>{
    let source=value.get("image_url").or_else(||value.get("imageUrl")).ok_or_else(invalid)?;
    let url=source.as_str().or_else(||source["url"].as_str()).filter(|url|!url.is_empty()&&url.len()<=8*1024*1024).ok_or_else(invalid)?;
    let valid=if let Some((prefix,data))=url.split_once(','){
        matches!(prefix,"data:image/png;base64"|"data:image/jpeg;base64"|"data:image/jpg;base64"|"data:image/webp;base64")&&!data.is_empty()&&data.bytes().all(|byte|byte.is_ascii_alphanumeric()||b"+/=".contains(&byte))
    }else{url::Url::parse(url).ok().is_some_and(|url|url.scheme()=="https"&&url.port_or_known_default()==Some(443)&&url.username().is_empty()&&url.password().is_none()&&url.fragment().is_none())};
    if !valid||url.chars().any(char::is_control){return Err(invalid());}
    let detail=source["detail"].as_str().filter(|detail|matches!(*detail,"auto"|"low"|"high")).unwrap_or("auto");
    Ok(json!({"type":"image_url","imageUrl":{"url":url,"detail":detail}}))
}
fn invalid()->Error{Error::http(400,"CatPaw 图片需要有效的内联 PNG、JPEG、WebP，或可公开访问的 HTTPS 图片地址").with_code("unsupported_parameter")}
