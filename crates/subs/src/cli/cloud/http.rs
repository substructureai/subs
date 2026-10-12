use std::error::Error as _;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{header, Method, Response, StatusCode};
use serde::{de::DeserializeOwned, Serialize};

use crate::api::v1::ApiError;

use super::telemetry;

#[derive(Debug)]
pub struct Unreachable {
    pub url: String,
    kind: &'static str,
    detail: String,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.kind, self.url, self.detail)
    }
}

impl std::error::Error for Unreachable {}

#[derive(Debug)]
pub struct HttpStatus {
    pub status: StatusCode,
    pub url: String,
    pub code: Option<String>,
    from_api: bool,
    message: String,
    suffix: &'static str,
}

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.code.as_deref() {
            Some(c) if !c.is_empty() => format!("HTTP {} {}", self.status.as_u16(), c),
            _ => format!("HTTP {}", self.status.as_u16()),
        };
        write!(
            f,
            "{prefix} from {}: {}.{}",
            self.url, self.message, self.suffix
        )
    }
}

impl std::error::Error for HttpStatus {}

pub fn api_status_of(err: &anyhow::Error) -> Option<StatusCode> {
    err.downcast_ref::<HttpStatus>()
        .filter(|e| e.from_api)
        .map(|e| e.status)
}

pub type Reauth = Box<dyn Fn() -> BoxFuture<'static, Result<Option<String>>> + Send + Sync>;

pub struct CloudClient {
    base_url: String,
    token: RwLock<Option<String>>,
    http: reqwest::Client,
    reauth: Option<Reauth>,
    login: tokio::sync::Mutex<()>,
    default_org: Mutex<Option<String>>,
    default_project: Mutex<Option<String>>,
    defaults_probed: AtomicBool,
}

impl fmt::Debug for CloudClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl CloudClient {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> anyhow::Result<Self> {
        let user_agent = format!(
            "subs/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
        let mut default_headers = HeaderMap::new();
        if let Some(t) = telemetry::get() {
            if let Ok(v) = HeaderValue::from_str(&t.invocation_id) {
                default_headers.insert(HeaderName::from_static("x-subs-invocation-id"), v);
            }
            if let Ok(v) = HeaderValue::from_str(t.command) {
                default_headers.insert(HeaderName::from_static("x-subs-command"), v);
            }
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .default_headers(default_headers)
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: RwLock::new(token),
            http,
            reauth: None,
            login: tokio::sync::Mutex::new(()),
            default_org: Mutex::new(None),
            default_project: Mutex::new(None),
            defaults_probed: AtomicBool::new(false),
        })
    }

    pub fn with_reauth(mut self, reauth: Reauth) -> Self {
        self.reauth = Some(reauth);
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn default_org(&self) -> Option<String> {
        self.default_org
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn default_project(&self) -> Option<String> {
        self.default_project
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn needs_default_probe(&self) -> bool {
        !self.defaults_probed.swap(true, Ordering::Relaxed)
    }

    fn capture_defaults(&self, headers: &HeaderMap) {
        if let Some(v) = headers
            .get("x-substructure-org")
            .and_then(|v| v.to_str().ok())
        {
            *self.default_org.lock().unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
        }
        if let Some(v) = headers
            .get("x-substructure-project")
            .and_then(|v| v.to_str().ok())
        {
            *self
                .default_project
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(v.to_string());
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.base_url, path)
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, self.url(path))
    }

    fn token(&self) -> Option<String> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Response> {
        let again = req.try_clone();
        let sent_with = self.token();
        let res = self.send_once(req, sent_with.as_deref()).await?;
        if res.status() != StatusCode::UNAUTHORIZED {
            return Ok(res);
        }
        let Some(again) = again else { return Ok(res) };
        match self.login(sent_with.as_deref()).await? {
            Some(token) => self.send_once(again, Some(&token)).await,
            None => Ok(res),
        }
    }

    async fn send_once(
        &self,
        req: reqwest::RequestBuilder,
        token: Option<&str>,
    ) -> Result<Response> {
        let req = match token {
            Some(t) => req.header(header::AUTHORIZATION, format!("Bearer {t}")),
            None => req,
        };
        let res = req.send().await.map_err(|e| self.transport_error(e))?;
        self.capture_defaults(res.headers());
        Ok(res)
    }

    async fn login(&self, refused: Option<&str>) -> Result<Option<String>> {
        let Some(reauth) = &self.reauth else {
            return Ok(None);
        };
        let _guard = self.login.lock().await;
        let current = self.token();
        if current.is_some() && current.as_deref() != refused {
            return Ok(current);
        }
        let fresh = reauth().await?;
        if fresh.is_some() {
            *self.token.write().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
        }
        Ok(fresh)
    }

    fn transport_error(&self, e: reqwest::Error) -> anyhow::Error {
        let kind = if e.is_timeout() {
            "timed out reaching"
        } else if e.is_connect() {
            "could not connect to"
        } else if e.is_request() {
            "could not reach"
        } else {
            return anyhow::Error::new(e);
        };
        let mut deepest: Option<&dyn std::error::Error> = e.source();
        while let Some(next) = deepest.and_then(|s| s.source()) {
            deepest = Some(next);
        }
        let detail = deepest
            .map(|s| s.to_string())
            .unwrap_or_else(|| e.to_string());
        anyhow::Error::new(Unreachable {
            url: self.base_url.clone(),
            kind,
            detail,
        })
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let res = self.send(self.request(Method::GET, path)).await?;
        decode(&self.base_url, res).await
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let res = self
            .send(self.request(Method::POST, path).json(body))
            .await?;
        decode(&self.base_url, res).await
    }

    pub async fn patch_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let res = self
            .send(self.request(Method::PATCH, path).json(body))
            .await?;
        decode(&self.base_url, res).await
    }

    pub async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let res = self
            .send(self.request(Method::PUT, path).json(body))
            .await?;
        decode(&self.base_url, res).await
    }

    pub async fn put_discard<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let res = self
            .send(self.request(Method::PUT, path).json(body))
            .await?;
        check_status(&self.base_url, res).await?;
        Ok(())
    }

    pub async fn post_json_discard<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let res = self
            .send(self.request(Method::POST, path).json(body))
            .await?;
        check_status(&self.base_url, res).await?;
        Ok(())
    }

    pub async fn delete_discard(&self, path: &str) -> Result<()> {
        let res = self.send(self.request(Method::DELETE, path)).await?;
        check_status(&self.base_url, res).await?;
        Ok(())
    }

    pub async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let res = self
            .send(
                self.request(Method::POST, path)
                    .header(header::CONTENT_LENGTH, "0"),
            )
            .await?;
        decode(&self.base_url, res).await
    }

    pub async fn post_json_raw<B: Serialize>(&self, path: &str, body: &B) -> Result<Response> {
        self.send(self.request(Method::POST, path).json(body)).await
    }

    pub async fn stream_sse<F>(&self, path: &str, on_line: F) -> Result<()>
    where
        F: FnMut(&str),
    {
        self.stream(self.request(Method::GET, path), on_line).await
    }

    pub async fn post_sse<B: Serialize, F>(&self, path: &str, body: &B, on_line: F) -> Result<()>
    where
        F: FnMut(&str),
    {
        self.stream(self.request(Method::POST, path).json(body), on_line)
            .await
    }

    async fn stream<F>(&self, req: reqwest::RequestBuilder, mut on_line: F) -> Result<()>
    where
        F: FnMut(&str),
    {
        let res = self
            .send(req.header(header::ACCEPT, "text/event-stream"))
            .await?;
        let res = check_status(&self.base_url, res).await?;

        let mut stream = res.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| {
                format!("the connection to {} dropped mid-stream", self.base_url)
            })?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let s = std::str::from_utf8(&line[..line.len() - 1])
                    .context("non-UTF8 in SSE stream")?
                    .trim_end_matches('\r');
                on_line(s);
            }
        }
        Ok(())
    }
}

fn snippet(body: &str) -> String {
    let line = body.trim().lines().next().unwrap_or("").trim();
    match line.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

async fn decode<T: DeserializeOwned>(url: &str, res: Response) -> Result<T> {
    let res = check_status(url, res).await?;
    res.json::<T>().await.context("decoding response JSON")
}

async fn check_status(url: &str, res: Response) -> Result<Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }

    let path = res.url().path().to_string();
    let body_text = res.text().await.unwrap_or_default();
    let parsed: Option<ApiError> = serde_json::from_str(&body_text).ok();
    let from_api = parsed.is_some();
    let (code, message) = parsed
        .map(|b| (b.error.code, b.error.message))
        .unwrap_or((None, None));

    let message = match message {
        Some(m) => m.trim_end_matches('.').to_string(),
        None if !from_api && !body_text.trim().is_empty() => {
            format!("not an API response: \"{}\"", snippet(&body_text))
        }
        None if status == StatusCode::NOT_FOUND => format!("no such endpoint: {path}"),
        None => format!("no detail for {path}"),
    };
    let suffix = match status {
        StatusCode::UNAUTHORIZED if from_api => " Run `subs login`.",
        _ => "",
    };

    Err(anyhow::Error::new(HttpStatus {
        status,
        url: url.to_string(),
        code,
        from_api,
        message,
        suffix,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::HeaderMap as AxumHeaders;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;

    use super::*;

    #[derive(Default)]
    struct Calls {
        seen: Mutex<Vec<Option<String>>>,
        logins: AtomicUsize,
    }

    async fn guarded(State(calls): State<Arc<Calls>>, headers: AxumHeaders) -> impl IntoResponse {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        calls
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(auth.clone());
        match auth.as_deref() {
            Some("Bearer fresh") => (StatusCode::OK, "{\"ok\":true}").into_response(),
            _ => (
                StatusCode::UNAUTHORIZED,
                "{\"error\":{\"code\":\"UNAUTHORIZED\",\"message\":\"Not authenticated\"}}",
            )
                .into_response(),
        }
    }

    async fn serve(calls: Arc<Calls>) -> String {
        let app = Router::new()
            .route("/guarded", get(guarded))
            .with_state(calls);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn reauth(calls: Arc<Calls>, issues: Option<&'static str>) -> Reauth {
        Box::new(move || {
            let calls = calls.clone();
            Box::pin(async move {
                calls.logins.fetch_add(1, Ordering::Relaxed);
                Ok(issues.map(str::to_string))
            })
        })
    }

    #[derive(Debug, serde::Deserialize)]
    struct Ok_ {
        ok: bool,
    }

    #[tokio::test]
    async fn a_refused_credential_is_replaced_and_the_request_sent_again() {
        let calls = Arc::new(Calls::default());
        let url = serve(calls.clone()).await;
        let client = CloudClient::new(url, Some("stale".to_string()))
            .unwrap()
            .with_reauth(reauth(calls.clone(), Some("fresh")));

        let res: Ok_ = client.get("/guarded").await.expect("the second attempt");
        assert!(res.ok);
        assert_eq!(
            *calls.seen.lock().unwrap_or_else(|e| e.into_inner()),
            vec![
                Some("Bearer stale".to_string()),
                Some("Bearer fresh".to_string())
            ]
        );

        let res: Ok_ = client.get("/guarded").await.expect("no second login");
        assert!(res.ok);
        assert_eq!(calls.logins.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn nobody_to_log_in_leaves_the_refusal_to_the_caller() {
        let calls = Arc::new(Calls::default());
        let url = serve(calls.clone()).await;
        let client = CloudClient::new(url, Some("stale".to_string()))
            .unwrap()
            .with_reauth(reauth(calls.clone(), None));

        let err = client.get::<Ok_>("/guarded").await.expect_err("401");
        assert_eq!(api_status_of(&err), Some(StatusCode::UNAUTHORIZED));
        assert!(err.to_string().ends_with("Run `subs login`."));
        assert_eq!(
            calls.seen.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1,
            "no second attempt"
        );
    }

    #[tokio::test]
    async fn a_client_with_no_login_to_run_sends_once() {
        let calls = Arc::new(Calls::default());
        let url = serve(calls.clone()).await;
        let client = CloudClient::new(url, Some("stale".to_string())).unwrap();

        client.get::<Ok_>("/guarded").await.expect_err("401");
        assert_eq!(
            calls.seen.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );
        assert_eq!(calls.logins.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn one_login_serves_the_requests_refused_at_the_same_time() {
        let calls = Arc::new(Calls::default());
        let url = serve(calls.clone()).await;
        let client = CloudClient::new(url, Some("stale".to_string()))
            .unwrap()
            .with_reauth(reauth(calls.clone(), Some("fresh")));

        let (a, b) = tokio::join!(client.get::<Ok_>("/guarded"), client.get::<Ok_>("/guarded"));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            calls.logins.load(Ordering::Relaxed),
            1,
            "the second waits and takes what the first won"
        );
    }

    #[test]
    fn an_unreachable_server_names_the_url_and_the_reason() {
        let e = Unreachable {
            url: "http://127.0.0.1:1".to_string(),
            kind: "could not connect to",
            detail: "Connection refused (os error 61)".to_string(),
        };
        assert_eq!(
            e.to_string(),
            "could not connect to http://127.0.0.1:1: Connection refused (os error 61)"
        );
    }

    #[test]
    fn a_status_carries_the_api_code_and_only_the_api_gets_the_login_hint() {
        let from_api = HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            url: "https://api.example".to_string(),
            code: Some("unauthenticated".to_string()),
            from_api: true,
            message: "Not authenticated".to_string(),
            suffix: " Run `subs login`.",
        };
        assert_eq!(
            from_api.to_string(),
            "HTTP 401 unauthenticated from https://api.example: Not authenticated. \
             Run `subs login`."
        );

        let from_proxy = HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            url: "https://api.example".to_string(),
            code: None,
            from_api: false,
            message: "not an API response: \"<html>\"".to_string(),
            suffix: "",
        };
        assert_eq!(
            from_proxy.to_string(),
            "HTTP 401 from https://api.example: not an API response: \"<html>\"."
        );
    }

    #[test]
    fn a_body_that_is_not_the_api_is_cut_to_one_readable_line() {
        assert_eq!(snippet("  <html>\n<body>\n"), "<html>");
        assert_eq!(snippet(""), "");
        let long = "x".repeat(300);
        let cut = snippet(&long);
        assert_eq!(cut.chars().count(), 81);
        assert!(cut.ends_with('…'));
    }
}
