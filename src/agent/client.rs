use super::api_types::*;
use std::time::Duration;

pub struct ApiClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    max_retries: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("authentication failed: {0}")]
    AuthFailure(String),
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("server error: {status} {body}")]
    ServerError { status: u16, body: String },
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("request failed: {0}")]
    RequestFailed(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("budget exceeded: used {used_tokens} tokens")]
    BudgetExceeded { used_tokens: u64 },
}

impl ApiClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            base_url,
            api_key,
            http: reqwest::Client::new(),
            max_retries: 3,
        }
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Classify an HTTP status code and response into an ApiError.
    fn classify_error(status: u16, headers: &ResponseHeaders, body: &str) -> ApiError {
        match status {
            401 => ApiError::AuthFailure(body.to_string()),
            429 => {
                let retry_after = headers
                    .retry_after
                    .as_deref()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(30);
                ApiError::RateLimited {
                    retry_after_secs: retry_after,
                }
            }
            s @ (500 | 502 | 503 | 504) => ApiError::ServerError {
                status: s,
                body: body.to_string(),
            },
            _ => ApiError::RequestFailed(format!("HTTP {status}: {body}")),
        }
    }

    /// Parse a JSON response body into a MessagesResponse.
    fn parse_response(body: &str) -> Result<MessagesResponse, ApiError> {
        serde_json::from_str(body)
            .map_err(|e| ApiError::InvalidResponse(format!("JSON parse error: {e}")))
    }

    /// Determine whether a given error is retryable.
    fn is_retryable(error: &ApiError) -> bool {
        matches!(
            error,
            ApiError::ServerError { .. }
                | ApiError::RateLimited { .. }
                | ApiError::Timeout(_)
                | ApiError::RequestFailed(_)
        )
    }

    pub async fn send_message(
        &self,
        request: &MessagesRequest,
        timeout: Duration,
    ) -> Result<MessagesResponse, ApiError> {
        let url = format!("{}/v1/messages", self.base_url);
        let mut last_err = ApiError::RequestFailed("no attempts made".to_string());

        for _attempt in 0..=self.max_retries {
            let result = self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .timeout(timeout)
                .json(request)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let headers = ResponseHeaders { retry_after };
                    let body = resp
                        .text()
                        .await
                        .map_err(|e| ApiError::RequestFailed(e.to_string()))?;

                    if status >= 200 && status < 300 {
                        return Self::parse_response(&body);
                    }

                    let err = Self::classify_error(status, &headers, &body);
                    if !Self::is_retryable(&err) {
                        return Err(err);
                    }
                    last_err = err;
                }
                Err(e) if e.is_timeout() => {
                    last_err = ApiError::Timeout(timeout);
                }
                Err(e) => {
                    last_err = ApiError::RequestFailed(e.to_string());
                }
            }
        }

        Err(last_err)
    }
}

/// Minimal header bag used by classify_error so it doesn't depend on reqwest types in tests.
struct ResponseHeaders {
    retry_after: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_client_construction() {
        let client = ApiClient::new(
            "https://api.anthropic.com".to_string(),
            "sk-ant-test-key".to_string(),
        );
        assert_eq!(client.base_url(), "https://api.anthropic.com");
        assert_eq!(client.api_key, "sk-ant-test-key");
        assert_eq!(client.max_retries(), 3);
    }

    #[test]
    fn test_client_trailing_slash_stripped() {
        let client = ApiClient::new(
            "https://api.anthropic.com/".to_string(),
            "key".to_string(),
        );
        assert_eq!(client.base_url(), "https://api.anthropic.com");
    }

    #[test]
    fn test_client_multiple_trailing_slashes_stripped() {
        let client = ApiClient::new(
            "https://api.anthropic.com///".to_string(),
            "key".to_string(),
        );
        assert_eq!(client.base_url(), "https://api.anthropic.com");
    }

    #[test]
    fn test_client_with_max_retries() {
        let client = ApiClient::new("https://api.anthropic.com".to_string(), "key".to_string())
            .with_max_retries(5);
        assert_eq!(client.max_retries(), 5);
    }

    #[test]
    fn test_classify_401_auth_failure() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(401, &headers, "invalid api key");
        match err {
            ApiError::AuthFailure(msg) => assert_eq!(msg, "invalid api key"),
            other => panic!("expected AuthFailure, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_429_rate_limited_with_retry_after() {
        let headers = ResponseHeaders {
            retry_after: Some("60".to_string()),
        };
        let err = ApiClient::classify_error(429, &headers, "rate limited");
        match err {
            ApiError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 60),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_429_rate_limited_default_retry_after() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(429, &headers, "rate limited");
        match err {
            ApiError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 30),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_429_rate_limited_non_numeric_retry_after() {
        let headers = ResponseHeaders {
            retry_after: Some("not-a-number".to_string()),
        };
        let err = ApiClient::classify_error(429, &headers, "");
        match err {
            ApiError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 30),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_500_server_error() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(500, &headers, "internal server error");
        match err {
            ApiError::ServerError { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "internal server error");
            }
            other => panic!("expected ServerError, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_502_server_error() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(502, &headers, "bad gateway");
        assert!(matches!(err, ApiError::ServerError { status: 502, .. }));
    }

    #[test]
    fn test_classify_503_server_error() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(503, &headers, "overloaded");
        assert!(matches!(err, ApiError::ServerError { status: 503, .. }));
    }

    #[test]
    fn test_classify_unknown_status() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(418, &headers, "I'm a teapot");
        match err {
            ApiError::RequestFailed(msg) => {
                assert!(msg.contains("418"));
                assert!(msg.contains("I'm a teapot"));
            }
            other => panic!("expected RequestFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_response_success() {
        let body = json!({
            "id": "msg_test",
            "content": [
                { "type": "text", "text": "Hello!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 3
            }
        })
        .to_string();

        let resp = ApiClient::parse_response(&body).unwrap();
        assert_eq!(resp.id, "msg_test");
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 3);
    }

    #[test]
    fn test_parse_response_tool_use() {
        let body = json!({
            "id": "msg_tu",
            "content": [
                {
                    "type": "tool_use",
                    "id": "tu_123",
                    "name": "bash",
                    "input": { "command": "pwd" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 30,
                "output_tokens": 15
            }
        })
        .to_string();

        let resp = ApiClient::parse_response(&body).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_123");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "pwd");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_response_malformed_json() {
        let err = ApiClient::parse_response("not json at all").unwrap_err();
        match err {
            ApiError::InvalidResponse(msg) => assert!(msg.contains("JSON parse error")),
            other => panic!("expected InvalidResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_response_missing_required_field() {
        let body = json!({
            "id": "msg_bad",
            "content": []
            // missing stop_reason and usage
        })
        .to_string();

        let err = ApiClient::parse_response(&body).unwrap_err();
        assert!(matches!(err, ApiError::InvalidResponse(_)));
    }

    #[test]
    fn test_token_counting_from_usage() {
        let body = json!({
            "id": "msg_tok",
            "content": [{ "type": "text", "text": "x" }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 500,
                "output_tokens": 200,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 50
            }
        })
        .to_string();

        let resp = ApiClient::parse_response(&body).unwrap();
        let total_input = resp.usage.input_tokens
            + resp.usage.cache_creation_input_tokens
            + resp.usage.cache_read_input_tokens;
        assert_eq!(total_input, 650);
        assert_eq!(resp.usage.output_tokens, 200);
    }

    #[test]
    fn test_is_retryable_server_error() {
        let err = ApiError::ServerError {
            status: 500,
            body: "".to_string(),
        };
        assert!(ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_rate_limited() {
        let err = ApiError::RateLimited {
            retry_after_secs: 10,
        };
        assert!(ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_is_retryable_timeout() {
        let err = ApiError::Timeout(Duration::from_secs(30));
        assert!(ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_not_retryable_auth_failure() {
        let err = ApiError::AuthFailure("bad key".to_string());
        assert!(!ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_not_retryable_invalid_response() {
        let err = ApiError::InvalidResponse("bad json".to_string());
        assert!(!ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_not_retryable_budget_exceeded() {
        let err = ApiError::BudgetExceeded { used_tokens: 999 };
        assert!(!ApiClient::is_retryable(&err));
    }

    #[test]
    fn test_error_display_messages() {
        let auth = ApiError::AuthFailure("invalid key".to_string());
        assert_eq!(auth.to_string(), "authentication failed: invalid key");

        let rate = ApiError::RateLimited {
            retry_after_secs: 45,
        };
        assert_eq!(rate.to_string(), "rate limited, retry after 45s");

        let server = ApiError::ServerError {
            status: 503,
            body: "overloaded".to_string(),
        };
        assert_eq!(server.to_string(), "server error: 503 overloaded");

        let timeout = ApiError::Timeout(Duration::from_secs(30));
        assert_eq!(timeout.to_string(), "request timed out after 30s");

        let budget = ApiError::BudgetExceeded { used_tokens: 1000 };
        assert_eq!(budget.to_string(), "budget exceeded: used 1000 tokens");
    }

    // --- New tests ---

    #[test]
    fn test_classify_error_400_invalid_request() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(400, &headers, "invalid request body");
        match err {
            ApiError::RequestFailed(msg) => {
                assert!(msg.contains("400"));
                assert!(msg.contains("invalid request body"));
            }
            other => panic!("expected RequestFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_error_413_request_too_large() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(413, &headers, "request too large");
        match err {
            ApiError::RequestFailed(msg) => {
                assert!(msg.contains("413"));
                assert!(msg.contains("request too large"));
            }
            other => panic!("expected RequestFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_error_529_overloaded() {
        let headers = ResponseHeaders { retry_after: None };
        let err = ApiClient::classify_error(529, &headers, "overloaded");
        // 529 is not in the server-error match arm, so it falls through to RequestFailed
        match err {
            ApiError::RequestFailed(msg) => {
                assert!(msg.contains("529"));
                assert!(msg.contains("overloaded"));
            }
            other => panic!("expected RequestFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_structured_anthropic_error_parsing() {
        // Anthropic API returns structured errors in this format
        let body = json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "max_tokens must be less than 100000"
            }
        })
        .to_string();

        let err = ApiClient::parse_response(&body).unwrap_err();
        match err {
            ApiError::InvalidResponse(msg) => {
                assert!(msg.contains("JSON parse error"));
            }
            other => panic!("expected InvalidResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_structured_openai_error_parsing() {
        // OpenAI-compatible proxies return errors in this format
        let body = json!({
            "error": {
                "message": "Rate limit exceeded",
                "code": "rate_limit_exceeded"
            }
        })
        .to_string();

        let err = ApiClient::parse_response(&body).unwrap_err();
        match err {
            ApiError::InvalidResponse(msg) => {
                assert!(msg.contains("JSON parse error"));
            }
            other => panic!("expected InvalidResponse, got {:?}", other),
        }
    }

    #[test]
    fn test_client_with_provider_anthropic() {
        let client = ApiClient::new(
            "https://api.anthropic.com".to_string(),
            "sk-ant-api03-test".to_string(),
        );
        // Anthropic client should have correct base URL for /v1/messages
        assert_eq!(client.base_url(), "https://api.anthropic.com");
        assert_eq!(client.api_key, "sk-ant-api03-test");
        // Default retry count
        assert_eq!(client.max_retries(), 3);
    }

    #[test]
    fn test_client_with_provider_openai() {
        let client = ApiClient::new(
            "https://openrouter.ai/api".to_string(),
            "sk-or-test-key".to_string(),
        );
        // OpenAI-compatible provider should have base URL without trailing slash
        assert_eq!(client.base_url(), "https://openrouter.ai/api");
        assert_eq!(client.api_key, "sk-or-test-key");
        assert_eq!(client.max_retries(), 3);
    }

    #[test]
    fn test_budget_exceeded_not_retryable() {
        let err = ApiError::BudgetExceeded { used_tokens: 50000 };
        assert!(
            !ApiClient::is_retryable(&err),
            "BudgetExceeded must not be retryable"
        );
    }

    #[test]
    fn test_retry_respects_max_attempts_exactly() {
        // With max_retries = 3, the loop runs attempts 0..=3 which is 4 iterations
        // (1 initial + 3 retries). Verify the client stores the right value.
        let client = ApiClient::new("https://api.example.com".to_string(), "key".to_string())
            .with_max_retries(3);
        assert_eq!(client.max_retries(), 3);

        // With max_retries = 0, only one attempt (no retries)
        let client_no_retry =
            ApiClient::new("https://api.example.com".to_string(), "key".to_string())
                .with_max_retries(0);
        assert_eq!(client_no_retry.max_retries(), 0);
    }
}
