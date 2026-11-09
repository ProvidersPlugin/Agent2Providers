//! Kiro 登录：本机监听与远程回填共用同一轮 state、PKCE 和串行换码。
//! 门户中的 AWS 登录有两次回调；直接 AWS 入口只有最终授权回调。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::LoginCallbackResult;
use serde_json::{json, Value};
use tokio::sync::watch;

use super::credentials::{identity, public};
use super::login_callback::{self, Request, Response};
use super::login_oauth::{self as oauth, AwsPending};
use super::{endpoints, login_methods};
use crate::logging;

#[derive(Default)]
struct FlowState {
    proxy: String,
    state: String,
    verifier: String,
    redirect: String,
    aws_redirect: String,
    portal: bool,
    aws: Option<AwsPending>,
    result: Option<Result<Value, String>>,
    hint: String,
}

struct Flow {
    inner: Mutex<FlowState>,
    operation: tokio::sync::Mutex<()>,
    stop: watch::Sender<bool>,
    deadline: tokio::time::Instant,
    port: u16,
}

impl Flow {
    fn new(port: u16, state: FlowState) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            inner: Mutex::new(state),
            operation: tokio::sync::Mutex::new(()),
            stop,
            port,
            deadline: tokio::time::Instant::now()
                + Duration::from_millis(endpoints::SIGN_IN_TIMEOUT_MS),
        }
    }

    fn with<T>(&self, action: impl FnOnce(&mut FlowState) -> T) -> T {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        action(&mut state)
    }

    fn finish(&self, result: Result<Value, String>) {
        self.with(|state| {
            if state.result.is_none() {
                state.result = Some(result);
            }
        });
        self.stop.send_replace(true);
    }

    fn active(&self) -> bool {
        self.with(|state| state.result.is_none()) && tokio::time::Instant::now() < self.deadline
    }
}

fn with_flows<T>(action: impl FnOnce(&mut HashMap<String, Arc<Flow>>) -> T) -> T {
    static FLOWS: OnceLock<Mutex<HashMap<String, Arc<Flow>>>> = OnceLock::new();
    let mut flows = FLOWS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    action(&mut flows)
}

pub async fn start(
    proxy: Option<&proto::WireProxy>,
    inputs: &Value,
) -> Result<proto::LoginStart, String> {
    let choice = inputs
        .get("loginMethod")
        .and_then(Value::as_str)
        .unwrap_or("portal");
    if !matches!(choice, "portal" | "builderid" | "enterprise") {
        return Err("请选择有效的 Kiro 登录方式".into());
    }
    let issuer = if choice == "enterprise" {
        inputs
            .get("startUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    } else {
        endpoints::BUILDER_ID_START
    };
    let region = inputs
        .get("region")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1");
    if choice != "portal" {
        if !endpoints::is_https_url(issuer) {
            return Err("请填写组织的 HTTPS 登录地址".into());
        }
        if !endpoints::is_aws_region(region) {
            return Err("请填写有效的 AWS 地区（例如 us-east-1）".into());
        }
    }
    let flow_id = format!("kiro-{}", oauth::random_token(24)?);
    let state = oauth::random_token(24)?;
    let verifier = oauth::random_token(48)?;
    let (listener, port) = listen().await?;
    let redirect = format!("http://localhost:{port}");
    let aws_redirect = format!("http://127.0.0.1:{port}/oauth/callback");
    let proxy = proxy
        .map(agent2api_provider_sdk::egress::proxy_url)
        .unwrap_or_default();
    let mut data = FlowState {
        proxy: proxy.clone(),
        state: state.clone(),
        verifier: verifier.clone(),
        redirect: redirect.clone(),
        aws_redirect: aws_redirect.clone(),
        portal: choice == "portal",
        hint: login_methods::WAIT_HINT.into(),
        ..Default::default()
    };
    let url = if choice == "portal" {
        oauth::portal_url(&state, &verifier, &redirect)
    } else {
        let option = if choice == "enterprise" {
            "awsidc"
        } else {
            "builderid"
        };
        let aws = oauth::start_aws(option, issuer, region, &aws_redirect, &proxy).await?;
        let url = aws.authorize_url.clone();
        data.aws = Some(aws);
        url
    };
    let flow = Arc::new(Flow::new(port, data));
    with_flows(|flows| flows.insert(flow_id.clone(), flow.clone()));
    spawn_listener(listener, flow.clone());
    let timeout_id = flow_id.clone();
    let mut stop = flow.stop.subscribe();
    tokio::spawn(async move {
        if *stop.borrow() {
            return;
        }
        tokio::select! {
            _ = stop.changed() => {}
            _ = tokio::time::sleep_until(flow.deadline) => {
                flow.finish(Err("登录超时，请重新发起".into()));
                with_flows(|flows| flows.remove(&timeout_id));
            }
        }
    });
    Ok(proto::LoginStart {
        flow: flow_id,
        url,
        instructions: login_methods::INSTRUCTIONS.into(),
        poll_interval_ms: 1500,
        external_browser: true,
        allowed_hosts: vec![
            "app.kiro.dev".into(),
            "awsapps.com".into(),
            "amazonaws.com".into(),
            "signin.aws".into(),
        ],
    })
}

pub async fn poll(flow_id: &str) -> proto::LoginPoll {
    let Some(flow) = with_flows(|flows| flows.get(flow_id).cloned()) else {
        return proto::LoginPoll::Failed {
            message: "这次登录已失效，请重新发起".into(),
        };
    };
    let poll = progress(&flow);
    if !matches!(poll, proto::LoginPoll::Pending { .. }) {
        with_flows(|flows| flows.remove(flow_id));
    }
    poll
}

fn progress(flow: &Flow) -> proto::LoginPoll {
    flow.with(|state| match &state.result {
        Some(Ok(secret)) => proto::LoginPoll::Done {
            account: proto::NormalizedAccount {
                identity: identity(secret),
                public: public(secret),
                secret: secret.clone(),
                desktop: false,
            },
        },
        Some(Err(message)) => proto::LoginPoll::Failed {
            message: message.clone(),
        },
        None => proto::LoginPoll::Pending {
            hint: Some(state.hint.clone()),
        },
    })
}

pub fn cancel(flow_id: &str) {
    if let Some(flow) = with_flows(|flows| flows.remove(flow_id)) {
        flow.finish(Err("登录已取消".into()));
    }
}

pub async fn callback(callback: proto::LoginCallback) -> LoginCallbackResult {
    let Some(flow) = with_flows(|flows| flows.get(&callback.flow).cloned()) else {
        return proto::LoginPoll::Failed {
            message: "这次登录已失效，请重新发起".into(),
        }
        .into();
    };
    let outcome = match Request::parse(&callback.url, flow.port) {
        Ok(request) => handle(&flow, request).await,
        Err(message) => Outcome::Rejected(message),
    };
    match outcome {
        Outcome::ContinueTo(next_url) => LoginCallbackResult {
            poll: proto::LoginPoll::Pending {
                hint: Some(login_methods::AWS_HINT.into()),
            },
            next_url: Some(next_url),
            retryable: false,
        },
        Outcome::Rejected(message) => LoginCallbackResult {
            poll: proto::LoginPoll::Pending {
                hint: Some(message),
            },
            next_url: None,
            retryable: true,
        },
        Outcome::Complete | Outcome::Failed(_) => progress(&flow).into(),
    }
}

pub fn immediate(secret: Value) -> Result<proto::LoginStart, String> {
    let flow_id = format!("kiro-local-{}", oauth::random_token(24)?);
    let flow = Arc::new(Flow::new(
        0,
        FlowState {
            result: Some(Ok(secret)),
            ..Default::default()
        },
    ));
    with_flows(|flows| flows.insert(flow_id.clone(), flow));
    Ok(proto::LoginStart {
        flow: flow_id,
        url: String::new(),
        instructions: "已读取本机 Kiro 登录态。".into(),
        poll_interval_ms: 1000,
        external_browser: false,
        allowed_hosts: Vec::new(),
    })
}

pub async fn start_local() -> Result<proto::LoginStart, String> {
    let credentials = super::refresh::read_local("kiro").await.ok_or_else(|| {
        "没有找到 Kiro 的本机登录态：请先在 kiro-cli 或 Kiro IDE 里登录".to_string()
    })?;
    let name = if !credentials.account_id.is_empty() {
        credentials.account_id.clone()
    } else if credentials.source == "kiro-cli" {
        "kiro-cli 的账号".into()
    } else {
        "Kiro IDE 的账号".into()
    };
    let mut secret = json!({ "name": name });
    credentials.write_into(&mut secret);
    immediate(secret)
}

async fn listen() -> Result<(tokio::net::TcpListener, u16), String> {
    for port in endpoints::CALLBACK_PORTS {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            return Ok((listener, *port));
        }
    }
    Err("Kiro 登录要用的本机端口都被占用了，请关闭 Kiro IDE 的登录后重试".into())
}

fn spawn_listener(listener: tokio::net::TcpListener, flow: Arc<Flow>) {
    let mut stop = flow.stop.subscribe();
    let connections = Arc::new(tokio::sync::Semaphore::new(8));
    tokio::spawn(async move {
        loop {
            if *stop.borrow() {
                break;
            }
            let accepted = tokio::select! {
                biased;
                _ = stop.changed() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else { break };
            let Ok(permit) = connections.clone().try_acquire_owned() else {
                continue;
            };
            let flow = flow.clone();
            let mut stop = flow.stop.subscribe();
            tokio::spawn(async move {
                let _permit = permit;
                if *stop.borrow() {
                    return;
                }
                let request = tokio::select! {
                    _ = stop.changed() => return,
                    result = tokio::time::timeout(Duration::from_secs(5), login_callback::read_request(&mut stream, flow.port)) => {
                        result.unwrap_or_else(|_| Err("读取登录回调超时".into()))
                    }
                };
                let response = match request {
                    Ok(request) => match handle(&flow, request).await {
                        Outcome::Complete => Response::page(
                            200,
                            "授权已完成",
                            "请返回 Agent2API 等待账号添加完成，可以关闭这个标签页。",
                        ),
                        Outcome::ContinueTo(url) => Response::redirect(url),
                        Outcome::Rejected(message) => Response::page(400, "回调未被接受", &message),
                        Outcome::Failed(message) => Response::page(410, "登录没有完成", &message),
                    },
                    Err(message) => Response::page(400, "回调未被接受", &message),
                };
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    login_callback::write_response(&mut stream, &response),
                )
                .await;
            });
        }
    });
}

enum Outcome {
    Complete,
    ContinueTo(String),
    Rejected(String),
    Failed(String),
}

async fn while_active<T>(
    flow: &Flow,
    work: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let mut stop = flow.stop.subscribe();
    if *stop.borrow() || !flow.active() {
        return Err("这次登录已结束，请重新发起".into());
    }
    tokio::select! {
        biased;
        _ = stop.changed() => Err("这次登录已结束，请重新发起".into()),
        _ = tokio::time::sleep_until(flow.deadline) => Err("登录超时，请重新发起".into()),
        result = work => result,
    }
}

async fn handle(flow: &Flow, request: Request) -> Outcome {
    let _operation = flow.operation.lock().await;
    let (portal, aws, proxy, verifier, redirect, aws_redirect, result) = flow.with(|state| {
        (
            state.portal && !state.state.is_empty() && request.value("state") == state.state,
            state.aws.clone(),
            state.proxy.clone(),
            state.verifier.clone(),
            state.redirect.clone(),
            state.aws_redirect.clone(),
            state.result.clone(),
        )
    });
    let from_aws = aws
        .as_ref()
        .is_some_and(|aws| request.value("state") == aws.state);
    if (!portal && !from_aws)
        || (portal && request.url.host_str() != Some("localhost"))
        || (from_aws
            && (request.url.host_str() != Some("127.0.0.1")
                || request.url.path() != "/oauth/callback"))
    {
        return Outcome::Rejected(
            "这个地址不属于本次登录，请复制刚打开的登录页的完整回调地址".into(),
        );
    }
    if let Some(result) = result {
        return match result {
            Ok(_) => Outcome::Complete,
            Err(message) => Outcome::Failed(message),
        };
    }
    if !flow.active() {
        return fail(flow, "登录超时，请重新发起".into());
    }
    if !request.value("error").is_empty() {
        return fail(flow, "授权未完成或已被拒绝，请重新发起登录".into());
    }
    if from_aws {
        let Some(aws) = aws else {
            return fail(flow, "AWS 登录状态已失效".into());
        };
        let code = request.value("code");
        if code.is_empty() {
            return Outcome::Rejected("AWS 回调缺少授权码，请复制完整地址".into());
        }
        flow.with(|state| state.hint = "已收到授权回调，正在完成登录…".into());
        return finish_exchange(
            flow,
            &proxy,
            oauth::exchange_aws(&aws, code, &aws_redirect, &proxy),
        )
        .await;
    }
    let option = request.value("login_option");
    let choice = (
        option.to_string(),
        request.value("issuer_url").to_string(),
        request.value("idc_region").to_string(),
    );
    if let Some(aws) = &aws {
        if aws.portal_choice.as_ref() == Some(&choice) {
            return Outcome::ContinueTo(aws.authorize_url.clone());
        }
    }
    match option {
        "google" | "github" => {
            let code = request.value("code");
            if code.is_empty() {
                return Outcome::Rejected("Kiro 回调缺少授权码，请复制完整地址".into());
            }
            flow.with(|state| {
                state.aws = None;
                state.hint = "已收到授权回调，正在完成登录…".into();
            });
            let redirect = format!("{redirect}{}?login_option={option}", request.url.path());
            finish_exchange(
                flow,
                &proxy,
                oauth::exchange_social(option, code, &verifier, &redirect, &proxy),
            )
            .await
        }
        "builderid" | "awsidc" | "internal" => {
            if !endpoints::is_https_url(&choice.1) || !endpoints::is_aws_region(&choice.2) {
                return Outcome::Rejected("AWS 登录地址或地区无效，请重新复制完整回调地址".into());
            }
            flow.with(|state| {
                state.aws = None;
                state.hint = "正在准备 AWS 授权…".into();
            });
            let authorize = while_active(
                flow,
                oauth::start_aws(option, &choice.1, &choice.2, &aws_redirect, &proxy),
            )
            .await;
            match authorize {
                Ok(mut aws) => {
                    let url = aws.authorize_url.clone();
                    aws.portal_choice = Some(choice);
                    let accepted = flow.with(|state| {
                        if state.result.is_some() {
                            return false;
                        }
                        state.aws = Some(aws);
                        state.hint = login_methods::AWS_HINT.into();
                        true
                    });
                    if accepted {
                        Outcome::ContinueTo(url)
                    } else {
                        Outcome::Failed("这次登录已结束，请重新发起".into())
                    }
                }
                Err(message) => fail(flow, message),
            }
        }
        "external_idp" => fail(
            flow,
            "公司自建身份提供商暂不支持此网页登录，请使用网关所在机器的 kiro-cli 登录态".into(),
        ),
        _ => Outcome::Rejected("回调没有有效的登录方式，请复制本次登录的完整地址".into()),
    }
}

async fn finish_exchange(
    flow: &Flow,
    proxy: &str,
    exchange: impl Future<Output = Result<super::credentials::Credentials, String>>,
) -> Outcome {
    let credentials = match while_active(flow, exchange).await {
        Ok(credentials) => credentials,
        Err(message) => return fail(flow, message),
    };
    let mut fallback = json!({ "name": "Kiro 账号" });
    credentials.write_into(&mut fallback);
    let enrich = async {
        Ok(
            tokio::time::timeout(Duration::from_secs(15), oauth::account(credentials, proxy))
                .await
                .unwrap_or(fallback),
        )
    };
    match while_active(flow, enrich).await {
        Ok(secret) => {
            if !flow.active() {
                return fail(flow, "这次登录已结束，请重新发起".into());
            }
            flow.finish(Ok(secret));
            match progress(flow) {
                proto::LoginPoll::Done { .. } => {
                    logging::verbose("kiro", "浏览器授权完成");
                    Outcome::Complete
                }
                proto::LoginPoll::Failed { message } => Outcome::Failed(message),
                _ => Outcome::Failed("登录尚未完成".into()),
            }
        }
        Err(message) => fail(flow, message),
    }
}

fn fail(flow: &Flow, message: String) -> Outcome {
    flow.finish(Err(message.clone()));
    Outcome::Failed(message)
}
