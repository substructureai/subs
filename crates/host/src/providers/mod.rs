pub mod anthropic;
pub mod format;
pub mod openai;
pub mod openrouter;
pub mod sqlite;

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

const TOO_LONG: &[&str] = &[
    "prompt is too long",
    "context_length_exceeded",
    "maximum context length",
    "context window",
    "too many tokens",
    "input is too long",
    "request_too_large",
];

pub(crate) fn too_long(status: u16, body: &str) -> bool {
    let body = body.to_lowercase();
    status == 413 || (status == 400 && TOO_LONG.iter().any(|p| body.contains(p)))
}

pub(crate) fn classify_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> crate::llm::LlmCallError {
    use crate::protocol::ErrorCode;
    let status_code = status.as_u16();
    let retryable = status.is_server_error() || status_code == 408 || status_code == 429;
    let code = match status_code {
        429 => ErrorCode::RateLimited,
        _ if too_long(status_code, body) => ErrorCode::ContextOverflow,
        _ => ErrorCode::ProviderError,
    };
    let message = match crate::json::error_message(body.as_bytes()) {
        Some(reported) => format!("{provider} API error {status}: {reported}"),
        None => format!("{provider} API error {status}"),
    };
    crate::llm::LlmCallError::new(code, message, retryable)
}

pub(crate) fn sse_data(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
}

#[cfg(test)]
mod tests {
    use super::{classify_error, sse_data};
    use crate::protocol::ErrorCode;

    #[test]
    fn a_request_longer_than_the_model_reads_is_an_overflow() {
        let code = |status: u16, body: &str| {
            let status = reqwest::StatusCode::from_u16(status).unwrap();
            classify_error("P", status, body).error.code
        };
        let anthropic = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        let openai = r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","code":"context_length_exceeded"}}"#;
        assert_eq!(code(400, anthropic), ErrorCode::ContextOverflow);
        assert_eq!(code(400, openai), ErrorCode::ContextOverflow);
        assert_eq!(code(413, "{}"), ErrorCode::ContextOverflow);
        assert_eq!(
            code(400, r#"{"error":{"message":"bad model"}}"#),
            ErrorCode::ProviderError
        );
        assert_eq!(
            code(429, "too many tokens per minute"),
            ErrorCode::RateLimited
        );
    }

    #[test]
    fn a_data_line_reads_the_same_with_or_without_the_space() {
        assert_eq!(sse_data("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data("data:{\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(sse_data("data: [DONE]"), Some("[DONE]"));
        assert_eq!(sse_data("data:[DONE]"), Some("[DONE]"));
        assert_eq!(sse_data("event: message"), None);
        assert_eq!(sse_data(": keep-alive"), None);
    }

    #[test]
    fn only_the_first_space_goes() {
        assert_eq!(sse_data("data:  x"), Some(" x"));
    }
}
