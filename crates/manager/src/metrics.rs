use prometheus::{IntCounter, IntGauge, Histogram, HistogramOpts, Registry, Encoder, TextEncoder};
use std::fmt;

#[derive(Debug)]
pub enum MetricsError {
    Encode(String),
    Utf8(std::string::FromUtf8Error),
}

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricsError::Encode(msg) => write!(f, "{}", msg),
            MetricsError::Utf8(e) => write!(f, "prometheus encoder produced invalid UTF-8: {}", e),
        }
    }
}

impl std::error::Error for MetricsError {}

impl From<std::string::FromUtf8Error> for MetricsError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        MetricsError::Utf8(e)
    }
}

pub struct Metrics {
    pub registry: Registry,
    pub runs_total: IntCounter,
    pub runs_success: IntCounter,
    pub runs_failure: IntCounter,
    pub runs_timeout: IntCounter,
    pub prs_created: IntCounter,
    pub active_workers: IntGauge,
    pub run_duration: Histogram,
    pub run_cost: Histogram,
    pub webhooks_received: IntCounter,
    pub webhooks_triggered: IntCounter,
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let runs_total = IntCounter::new("autoanneal_runs_total", "Total worker runs started")?;
        let runs_success = IntCounter::new("autoanneal_runs_success_total", "Successful worker runs")?;
        let runs_failure = IntCounter::new("autoanneal_runs_failure_total", "Failed worker runs")?;
        let runs_timeout = IntCounter::new("autoanneal_runs_timeout_total", "Timed out worker runs")?;
        let prs_created = IntCounter::new("autoanneal_prs_created_total", "PRs created by workers")?;
        let active_workers = IntGauge::new("autoanneal_active_workers", "Currently running workers")?;
        let run_duration = Histogram::with_opts(
            HistogramOpts::new("autoanneal_run_duration_seconds", "Worker run duration")
                .buckets(vec![60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0, 3600.0])
        )?;
        let run_cost = Histogram::with_opts(
            HistogramOpts::new("autoanneal_run_cost_usd", "Worker run cost in USD")
                .buckets(vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0])
        )?;
        let webhooks_received = IntCounter::new("autoanneal_webhooks_received_total", "Webhooks received")?;
        let webhooks_triggered = IntCounter::new("autoanneal_webhooks_triggered_total", "Webhooks that triggered a run")?;

        registry.register(Box::new(runs_total.clone()))?;
        registry.register(Box::new(runs_success.clone()))?;
        registry.register(Box::new(runs_failure.clone()))?;
        registry.register(Box::new(runs_timeout.clone()))?;
        registry.register(Box::new(prs_created.clone()))?;
        registry.register(Box::new(active_workers.clone()))?;
        registry.register(Box::new(run_duration.clone()))?;
        registry.register(Box::new(run_cost.clone()))?;
        registry.register(Box::new(webhooks_received.clone()))?;
        registry.register(Box::new(webhooks_triggered.clone()))?;

        Ok(Self {
            registry,
            runs_total,
            runs_success,
            runs_failure,
            runs_timeout,
            prs_created,
            active_workers,
            run_duration,
            run_cost,
            webhooks_received,
            webhooks_triggered,
        })
    }

    pub fn render(&self) -> Result<String, MetricsError> {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .map_err(|e| MetricsError::Encode(format!("failed to encode metrics: {}", e)))?;
        String::from_utf8(buffer).map_err(MetricsError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_render() {
        let m = Metrics::new().unwrap();
        m.runs_total.inc();
        m.runs_success.inc();

        let output = m.render().unwrap();
        // Should be valid Prometheus text format with HELP and TYPE lines
        assert!(output.contains("# HELP"));
        assert!(output.contains("# TYPE"));
        // Counter values should appear
        assert!(output.contains("autoanneal_runs_total 1"));
        assert!(output.contains("autoanneal_runs_success_total 1"));
    }

    #[test]
    fn test_metrics_contain_expected_names() {
        let m = Metrics::new().unwrap();
        let output = m.render().unwrap();

        let expected_names = [
            "autoanneal_runs_total",
            "autoanneal_runs_success_total",
            "autoanneal_runs_failure_total",
            "autoanneal_runs_timeout_total",
            "autoanneal_prs_created_total",
            "autoanneal_active_workers",
            "autoanneal_run_duration_seconds",
            "autoanneal_run_cost_usd",
            "autoanneal_webhooks_received_total",
            "autoanneal_webhooks_triggered_total",
        ];

        for name in expected_names {
            assert!(output.contains(name), "missing metric: {name}");
        }
    }

    #[test]
    fn test_metrics_error_display() {
        let encode_err = MetricsError::Encode("test error".to_string());
        assert_eq!(encode_err.to_string(), "test error");

        let utf8_bytes = vec![0x80, 0x81, 0x82];
        let utf8_result = String::from_utf8(utf8_bytes);
        let utf8_err = MetricsError::Utf8(utf8_result.unwrap_err());
        assert!(utf8_err.to_string().contains("prometheus encoder produced invalid UTF-8"));
    }

    #[test]
    fn test_metrics_error_from_utf8() {
        let bytes = vec![0xff, 0xfe];
        let result = String::from_utf8(bytes);
        let err: MetricsError = result.unwrap_err().into();
        match err {
            MetricsError::Utf8(_) => (), // expected
            _ => panic!("expected Utf8 variant"),
        }
    }
}
