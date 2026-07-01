pub(crate) fn format_local_models(resp: &serde_json::Value) -> Vec<String> {
    let Some(models) = resp.get("models").and_then(|m| m.as_array()) else {
        return vec!["no local models".to_owned()];
    };
    if models.is_empty() {
        return vec!["no local models".to_owned()];
    }
    let mut lines = vec![format!(
        "{:<32}  {:>10}  {:<8}  {}",
        "MODEL", "EST_VRAM", "FIT", "ACTIVE"
    )];
    for m in models {
        let id = m["id"].as_str().unwrap_or("-");
        let vram = m["est_vram_mb"]
            .as_u64()
            .map_or_else(|| "-".to_owned(), |v| format!("{v} MiB"));
        let fit = m["fit"].as_str().unwrap_or("unknown");
        let active = if m["active"].as_bool().unwrap_or(false) {
            "*"
        } else {
            ""
        };
        lines.push(format!("{id:<32}  {vram:>10}  {fit:<8}  {active}"));
    }
    lines
}

pub(crate) fn format_local_gpu(resp: &serde_json::Value) -> String {
    if !resp["detected"].as_bool().unwrap_or(false) {
        return "no GPU detected".to_owned();
    }
    let device = resp["device"].as_str().unwrap_or("unknown");
    let total = resp["vram_total_mb"].as_u64().unwrap_or(0);
    let free = resp["vram_free_mb"].as_u64().unwrap_or(0);
    format!("{device}  VRAM {free}/{total} MiB free")
}

pub(crate) fn format_metrics_rows(
    resp: &serde_json::Value,
    runner_filter: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<22}  {:<12}  {:>5}  {:>9}  {:>9}  {:>11}  {:>6}",
        "BUCKET", "RUNNER", "TURNS", "INPUT", "OUTPUT", "COST", "ERRORS"
    )];
    let Some(buckets) = resp["buckets"].as_array() else {
        return lines;
    };
    for b in buckets {
        let runner = b["runner"].as_str().unwrap_or("-");
        if let Some(filter) = runner_filter {
            if runner != filter {
                continue;
            }
        }
        let bucket = format_bucket_start(b["bucket_start"].as_i64().unwrap_or(0));
        let turns = b["turns"].as_i64().unwrap_or(0);
        let input = b["input_tok"].as_i64().unwrap_or(0);
        let output = b["output_tok"].as_i64().unwrap_or(0);
        let cost = b["cost_usd"].as_f64().unwrap_or(0.0);
        let errors = b["error_count"].as_i64().unwrap_or(0);
        lines.push(format!(
            "{bucket:<22}  {runner:<12}  {turns:>5}  {input:>9}  {output:>9}  ${cost:>10.6}  {errors:>6}"
        ));
    }
    lines
}

pub(crate) fn format_bucket_start(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros).map_or_else(
        || micros.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M").to_string(),
    )
}

pub(crate) fn format_savings_rows(resp: &serde_json::Value) -> Vec<String> {
    let ratio = resp["efficiency_ratio"].as_f64().unwrap_or(0.0);
    let compression = resp["compression_saved"].as_i64().unwrap_or(0);
    let cache = resp["cache_saved"].as_i64().unwrap_or(0);
    let mut lines = vec![
        format!("EFFICIENCY  {:.1}%", ratio * 100.0),
        format!("COMPRESSION {compression} tok  (filter + crusher + cold-context)"),
        format!("CACHE       {cache} tok  (input not re-paid)"),
        String::new(),
        format!("{:<22}  {:<14}  {:>12}", "BUCKET", "SOURCE", "TOKENS_SAVED"),
    ];
    let Some(buckets) = resp["buckets"].as_array() else {
        return lines;
    };
    for b in buckets {
        let bucket = format_bucket_start(b["bucket_start"].as_i64().unwrap_or(0));
        let source = b["source"].as_str().unwrap_or("-");
        let saved = b["tokens_saved"].as_i64().unwrap_or(0);
        lines.push(format!("{bucket:<22}  {source:<14}  {saved:>12}"));
    }
    lines
}
