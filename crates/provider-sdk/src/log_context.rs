use std::future::Future;

use serde_json::{Map, Value};

tokio::task_local! {
    static CONTEXT: Value;
}

pub(crate) fn current() -> Value {
    CONTEXT.try_with(Clone::clone).unwrap_or(Value::Null)
}

pub(crate) fn capture(call_id: i64, method: &str, params: &Value) -> Value {
    let mut context = Map::new();
    context.insert("callId".to_string(), Value::from(call_id));
    context.insert("action".to_string(), Value::from(method));
    for key in ["provider", "account"] {
        if let Some(value) = params
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            context.insert(key.to_string(), Value::from(value));
        }
    }
    if let Some(model) = params
        .get("wire_model")
        .or_else(|| params.get("model"))
        .or_else(|| params.get("body").and_then(|body| body.get("model")))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        context.insert("model".to_string(), Value::from(model));
    }
    Value::Object(context)
}

pub(crate) async fn scope<F: Future>(context: Value, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}
