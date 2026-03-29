use prometheus::{IntCounter, IntGauge, Histogram, HistogramOpts, Registry, Encoder, TextEncoder};

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

    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }
}
