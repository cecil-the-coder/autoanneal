use super::api_types::*;
use super::conversation::{ApiError as ConvApiError, MessageSender};
use super::provider::Provider;
use std::time::Duration;

pub struct ApiClient {
    base_url: String,
    api_key: String,
    provider: Provider,
    /// When true, always use `Authorization: Bearer` regardless of provider.
    /// This supports Anthropic proxies/gateways that use bearer token auth
    /// (matching Claude Code's ANTHROPIC_AUTH_TOKEN behavior).
    use_bearer: bool,
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
}

impl ApiClient {
    pub fn new(base_url: String, api_key: String, provider: Provider, use_bearer: bool) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        Self {
            base_url,
            api_key,
            provider,
            use_bearer,
            http: reqwest::Client::new(),
            max_retries: 3,
        }
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
            s @ (500 | 502 | 503 | 504 | 529) => ApiError::ServerError {
                status: s,
                body: body.to_string(),
            },
            _ => ApiError::RequestFailed(format!("HTTP {status}: {body}")),
        }
    }

    /// Determine whether a given error is retryable.
    fn is_retryable(error: &ApiError) -> bool {
        matches!(
            error,
            ApiError::ServerError { .. }
                | ApiError::RateLimited { .. }
                | ApiError::Timeout(_)
        )
    }

    pub async fn send_message(
        &self,
        request: &MessagesRequest,
        timeout: Duration,
    ) -> Result<MessagesResponse, ApiError> {
        let body_json = self.provider.serialize_request(request);
        let url = format!("{}{}", self.base_url, self.provider.url_path());
        let auth_headers = self.provider.auth_headers(&self.api_key, self.use_bearer);

        let mut last_err = ApiError::RequestFailed("no attempts made".to_string());

        for attempt in 0..=self.max_retries {
            let mut req_builder = self.http.post(&url).timeout(timeout);
            for (key, value) in &auth_headers {
                req_builder = req_builder.header(*key, value);
            }
            req_builder = req_builder.json(&body_json);

            let result = req_builder.send().await;

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
                        return self
                            .provider
                            .deserialize_response(&body)
                            .map_err(|e| ApiError::InvalidResponse(format!("{e}")));
                    }

                    let err = Self::classify_error(status, &headers, &body);
                    if !Self::is_retryable(&err) {
                        return Err(err);
                    }

                    // Backoff before next retry (skip delay after last attempt).
                    if attempt < self.max_retries {
                        let delay = match &err {
                            ApiError::RateLimited { retry_after_secs } => {
                                Duration::from_secs(*retry_after_secs)
                            }
                            _ => Duration::from_secs(1 << attempt), // 1s, 2s, 4s
                        };
                        tokio::time::sleep(delay).await;
                    }

                    last_err = err;
                }
                Err(e) if e.is_timeout() => {
                    last_err = ApiError::Timeout(timeout);
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
                Err(e) => {
                    last_err = ApiError::RequestFailed(e.to_string());
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
            }
        }

        Err(last_err)
    }
}

#[async_trait::async_trait]
impl MessageSender for ApiClient {
    async fn send(
        &self,
        request: &MessagesRequest,
        timeout: Duration,
    ) -> std::result::Result<MessagesResponse, ConvApiError> {
        self.send_message(request, timeout).await.map_err(|e| match e {
            ApiError::Timeout(_) => ConvApiError::Timeout,
            ApiError::InvalidResponse(msg) => ConvApiError::MalformedResponse(msg),
            ApiError::AuthFailure(msg) => ConvApiError::Http {
                status: 401,
                body: msg,
            },
            ApiError::RateLimited { retry_after_secs } => ConvApiError::Http {
                status: 429,
                body: format!("rate limited, retry after {retry_after_secs}s"),
            },
            ApiError::ServerError { status, body } => ConvApiError::Http { status, body },
            ApiError::RequestFailed(msg) => ConvApiError::Request(msg),
        })
    }
}

/// Minimal header bag used by classify_error so it doesn't depend on reqwest types in tests.
struct ResponseHeaders {
    retry_after: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn test_client_construction() {
        let client = ApiClient::new(
            "https://api.anthropic.com".to_string(),
            "sk-ant-test-key".to_string(),
            Provider::Anthropic,
            false,
        );
        assert_eq!(client.base_url, "https://api.anthropic.com");
        assert_eq!(client.api_key, "sk-ant-test-key");
        assert_eq!(client.max_retries, 3);
    }

    #[test]
    fn test_client_trailing_slash_stripped() {
        let client = ApiClient::new(
            "https://api.anthropic.com/".to_string(),
            "key".to_string(),
            Provider::Anthropic,
            false,
        );
        assert_eq!(client.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn test_client_multiple_trailing_slashes_stripped() {
        let client = ApiClient::new(
            "https://api.anthropic.com///".to_string(),
            "key".to_string(),
            Provider::Anthropic,
            false,
        );
        assert_eq!(client.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn test_client_default_max_retries() {
        let client = ApiClient::new("https://api.anthropic.com".to_string(), "key".to_string(), Provider::Anthropic, false);
        assert_eq!(client.max_retries, 3);
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
        assert!(ApiClient::is_retryable(&err));
        match err {
            ApiError::ServerError { status, body } => {
                assert_eq!(status, 529);
                assert_eq!(body, "overloaded");
            }
            other => panic!("expected ServerError, got {:?}", other),
        }
    }

    #[test]
    fn test_client_with_provider_anthropic() {
        let client = ApiClient::new(
            "https://api.anthropic.com".to_string(),
            "sk-ant-api03-test".to_string(),
            Provider::Anthropic,
            false,
        );
        assert_eq!(client.base_url, "https://api.anthropic.com");
        assert_eq!(client.api_key, "sk-ant-api03-test");
        assert_eq!(client.max_retries, 3);
    }

    #[test]
    fn test_client_with_provider_openai() {
        let client = ApiClient::new(
            "https://openrouter.ai/api".to_string(),
            "sk-or-test-key".to_string(),
            Provider::OpenAi,
            true,
        );
        assert_eq!(client.base_url, "https://openrouter.ai/api");
        assert_eq!(client.api_key, "sk-or-test-key");
        assert_eq!(client.max_retries, 3);
    }

    #[test]
    fn test_default_retry_count() {
        // Default max_retries = 3, the loop runs attempts 0..=3 which is 4 iterations
        // (1 initial + 3 retries).
        let client = ApiClient::new("https://api.example.com".to_string(), "key".to_string(), Provider::Anthropic, false);
        assert_eq!(client.max_retries, 3);
    }
}
