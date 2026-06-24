//! GPU-detection probe for the local-model picker.
//!
//! smedja shells out to a vendor tool (`nvidia-smi`) and parses its CSV output
//! into an advisory [`GpuSnapshot`]; it does not link a GPU SDK and never gates
//! a swap on the result. A CPU-only or non-NVIDIA host degrades cleanly to
//! [`GpuSnapshot::none`] (an explicit "no GPU" shape, never an error that aborts
//! the daemon). GPU placement and load/unload stay with the external swap proxy.

use std::time::Duration;

use crate::local::LocalModel;

/// An advisory snapshot of the host's first GPU, or an explicit "no GPU" shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuSnapshot {
    /// The device name (e.g. `NVIDIA GeForce RTX 4090`), or `None` when absent.
    pub device: Option<String>,
    /// Total VRAM in MiB, or `None` when unknown.
    pub vram_total_mb: Option<u64>,
    /// Free VRAM in MiB, or `None` when unknown.
    pub vram_free_mb: Option<u64>,
}

impl GpuSnapshot {
    /// Returns the explicit "no GPU detected" snapshot — every field `None`.
    ///
    /// Used when the vendor tool is absent, reports zero GPUs, or its output
    /// cannot be parsed; the picker treats this as "unknown fit" rather than a
    /// failure.
    #[must_use]
    pub fn none() -> Self {
        Self {
            device: None,
            vram_total_mb: None,
            vram_free_mb: None,
        }
    }

    /// Returns `true` when no GPU was detected.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.device.is_none() && self.vram_total_mb.is_none() && self.vram_free_mb.is_none()
    }
}

/// The advisory fit of a model against a GPU snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// The model's estimated VRAM comfortably fits free VRAM.
    Fits,
    /// The model fits but leaves little headroom (within 10% of free VRAM).
    Tight,
    /// The model's estimated VRAM exceeds free VRAM.
    Exceeds,
    /// Fit cannot be computed (no GPU snapshot or no `est_vram_mb`).
    Unknown,
}

impl Fit {
    /// Returns the lowercase label used in RPC responses and the picker.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Fit::Fits => "fits",
            Fit::Tight => "tight",
            Fit::Exceeds => "exceeds",
            Fit::Unknown => "unknown",
        }
    }
}

/// Computes the advisory [`Fit`] of `model` against `snapshot`.
///
/// Returns [`Fit::Unknown`] when the model has no `est_vram_mb` or the snapshot
/// has no `vram_free_mb`. Otherwise compares the estimate to free VRAM:
/// `Exceeds` when over, `Tight` within the top 10% of free VRAM, else `Fits`.
#[must_use]
pub fn fit_for(model: &LocalModel, snapshot: &GpuSnapshot) -> Fit {
    let (Some(est), Some(free)) = (model.est_vram_mb, snapshot.vram_free_mb) else {
        return Fit::Unknown;
    };
    if est > free {
        return Fit::Exceeds;
    }
    // Within the top 10% of free VRAM counts as tight headroom.
    let tight_floor = free.saturating_sub(free / 10);
    if est >= tight_floor {
        Fit::Tight
    } else {
        Fit::Fits
    }
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,memory.free
/// --format=csv,noheader,nounits` output into a [`GpuSnapshot`].
///
/// Reads the first data row defensively: a missing or non-numeric memory field
/// yields `None` for that field rather than an error. Empty input yields
/// [`GpuSnapshot::none`].
#[must_use]
pub fn parse_gpu_snapshot(csv: &str) -> GpuSnapshot {
    let Some(line) = csv.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return GpuSnapshot::none();
    };
    let mut fields = line.split(',').map(str::trim);
    let device = fields
        .next()
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let vram_total_mb = fields.next().and_then(|s| s.parse::<u64>().ok());
    let vram_free_mb = fields.next().and_then(|s| s.parse::<u64>().ok());
    GpuSnapshot {
        device,
        vram_total_mb,
        vram_free_mb,
    }
}

/// Detects the host GPU by shelling out to `nvidia-smi`.
///
/// Returns [`GpuSnapshot::none`] when the tool is absent, exits non-zero, or its
/// output cannot be parsed — detection is advisory and never fails the caller.
/// The shell-out runs through `tokio::process` so it does not block the async
/// runtime.
pub async fn detect_gpu() -> GpuSnapshot {
    let span = tracing::info_span!("smedja.local.gpu_detect");
    let _enter = span.enter();

    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=name,memory.total,memory.free",
        "--format=csv,noheader,nounits",
    ]);

    let output = match tokio::time::timeout(Duration::from_secs(2), cmd.output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        Ok(Ok(_)) => {
            tracing::debug!("nvidia-smi exited non-zero — reporting no GPU");
            return GpuSnapshot::none();
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "nvidia-smi unavailable — reporting no GPU");
            return GpuSnapshot::none();
        }
        Err(_) => {
            tracing::debug!("nvidia-smi timed out — reporting no GPU");
            return GpuSnapshot::none();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_gpu_snapshot(&stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_gpu_snapshot` extracts device, total, and free from captured CSV.
    #[test]
    fn parse_gpu_snapshot_extracts_fields_from_nvidia_smi_csv() {
        // Captured `nvidia-smi --query-gpu=name,memory.total,memory.free
        // --format=csv,noheader,nounits` output.
        let csv = "NVIDIA GeForce RTX 4090, 24564, 21000\n";
        let snap = parse_gpu_snapshot(csv);
        assert_eq!(snap.device.as_deref(), Some("NVIDIA GeForce RTX 4090"));
        assert_eq!(snap.vram_total_mb, Some(24564));
        assert_eq!(snap.vram_free_mb, Some(21000));
    }

    /// Empty output (no GPU / tool absent shape) yields an explicit none snapshot.
    #[test]
    fn parse_gpu_snapshot_empty_input_yields_none() {
        let snap = parse_gpu_snapshot("");
        assert_eq!(snap, GpuSnapshot::none());
        assert!(
            snap.is_none(),
            "empty input must be the explicit no-GPU shape"
        );
    }

    /// A malformed memory field degrades to `None` for that field, not an error.
    #[test]
    fn parse_gpu_snapshot_degrades_malformed_field_to_none() {
        let snap = parse_gpu_snapshot("Some GPU, [N/A], 4096\n");
        assert_eq!(snap.device.as_deref(), Some("Some GPU"));
        assert_eq!(snap.vram_total_mb, None, "non-numeric total → None");
        assert_eq!(snap.vram_free_mb, Some(4096));
    }

    /// `detect_gpu` never errors on a host without `nvidia-smi`.
    #[tokio::test]
    async fn detect_gpu_returns_none_when_tool_absent() {
        // CI hosts generally lack nvidia-smi; the call must complete and degrade.
        let snap = detect_gpu().await;
        // We do not assert is_none() (a GPU host may be present); we assert the
        // call returns a well-formed snapshot without panicking.
        let _ = snap.is_none();
    }

    fn model(est: Option<u64>) -> LocalModel {
        LocalModel {
            id: "m".to_owned(),
            est_vram_mb: est,
        }
    }

    /// `fit_for` returns `Fits` when the estimate is well under free VRAM.
    #[test]
    fn fit_for_fits_when_under() {
        let snap = GpuSnapshot {
            device: Some("g".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        assert_eq!(fit_for(&model(Some(9000)), &snap), Fit::Fits);
    }

    /// `fit_for` returns `Tight` within the top 10% of free VRAM.
    #[test]
    fn fit_for_tight_near_ceiling() {
        let snap = GpuSnapshot {
            device: Some("g".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        // tight_floor = 20000 - 2000 = 18000; 19000 is in [18000, 20000].
        assert_eq!(fit_for(&model(Some(19000)), &snap), Fit::Tight);
    }

    /// `fit_for` returns `Exceeds` when the estimate is over free VRAM.
    #[test]
    fn fit_for_exceeds_when_over() {
        let snap = GpuSnapshot {
            device: Some("g".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        assert_eq!(fit_for(&model(Some(21000)), &snap), Fit::Exceeds);
    }

    /// `fit_for` returns `Unknown` when `est_vram_mb` is `None`.
    #[test]
    fn fit_for_unknown_without_estimate() {
        let snap = GpuSnapshot {
            device: Some("g".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        assert_eq!(fit_for(&model(None), &snap), Fit::Unknown);
    }

    /// `fit_for` returns `Unknown` when the snapshot has no free VRAM.
    #[test]
    fn fit_for_unknown_without_snapshot() {
        assert_eq!(
            fit_for(&model(Some(9000)), &GpuSnapshot::none()),
            Fit::Unknown
        );
    }
}
