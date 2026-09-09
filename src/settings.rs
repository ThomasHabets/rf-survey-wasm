use crate::model::{ScanPlan, WorkerConfig};
use wasm_bindgen::JsValue;
fn js_error(s: &str) -> JsValue {
    JsValue::from_str(s)
}
fn display_error(e: impl std::fmt::Display) -> JsValue {
    js_error(&e.to_string())
}
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct RequestedSettings {
    pub(crate) start_hz: f64,
    pub(crate) stop_hz: f64,
    pub(crate) sample_rate_hz: f64,
    pub(crate) step_hz: Option<f64>,
    pub(crate) dwell_ms: f64,
    pub(crate) settle_ms: f64,
    pub(crate) fft_size: usize,
    pub(crate) gain_db: f64,
    pub(crate) lo_offset_hz: Option<f64>,
    pub(crate) fixed_lo_offset: bool,
}

impl RequestedSettings {
    pub(crate) fn validate_before_usb(self) -> Result<Self, JsValue> {
        if !self.start_hz.is_finite() || self.start_hz <= 0.0 {
            return Err(js_error("Start frequency must be positive and finite"));
        }
        if !self.stop_hz.is_finite() || self.stop_hz <= self.start_hz {
            return Err(js_error(
                "Stop frequency must be greater than start frequency",
            ));
        }
        if !self.sample_rate_hz.is_finite() || self.sample_rate_hz <= 0.0 {
            return Err(js_error("Sample rate must be positive and finite"));
        }
        if !self.dwell_ms.is_finite() || self.dwell_ms <= 0.0 {
            return Err(js_error("Dwell must be positive and finite"));
        }
        if !self.settle_ms.is_finite() || self.settle_ms < 0.0 {
            return Err(js_error("Settle must be non-negative and finite"));
        }
        if self.settle_ms >= self.dwell_ms {
            return Err(js_error("Settle must be shorter than dwell"));
        }
        if !self.gain_db.is_finite() || !(0.0..=76.0).contains(&self.gain_db) {
            return Err(js_error("Gain must be between 0 and 76 dB"));
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedSettings {
    pub(crate) plan: ScanPlan,
    pub(crate) sample_rate_hz: f64,
    pub(crate) step_hz: f64,
    pub(crate) lo_offset_hz: f64,
    pub(crate) fixed_lo_offset: bool,
    pub(crate) dwell_samples: usize,
    pub(crate) settle_samples: usize,
    pub(crate) fft_size: usize,
}

impl ResolvedSettings {
    pub(crate) fn new(requested: RequestedSettings, actual_rate_hz: f64) -> Result<Self, JsValue> {
        let step_hz = requested.step_hz.unwrap_or(actual_rate_hz * 0.8);
        let plan = ScanPlan::new(
            requested.start_hz,
            requested.stop_hz,
            step_hz,
            actual_rate_hz,
        )
        .map_err(|error| js_error(&error))?;
        let retained_edge = step_hz / 2.0;
        let nyquist = actual_rate_hz / 2.0;
        let lo_offset_hz = requested
            .lo_offset_hz
            .unwrap_or((retained_edge + nyquist) / 2.0);
        if !lo_offset_hz.is_finite() {
            return Err(js_error("LO offset must be finite"));
        }
        if lo_offset_hz != 0.0
            && !(lo_offset_hz.abs() > retained_edge && lo_offset_hz.abs() < nyquist)
        {
            return Err(js_error(&format!(
                "Absolute LO offset must be above {:.6} MHz and below {:.6} MHz",
                retained_edge / 1e6,
                nyquist / 1e6
            )));
        }
        validate_tune_extremes(&plan, lo_offset_hz, requested.fixed_lo_offset)?;

        let dwell_samples = duration_samples(requested.dwell_ms, actual_rate_hz, "dwell")?;
        let settle_samples = duration_samples(requested.settle_ms, actual_rate_hz, "settle")?;
        let usable_samples = dwell_samples.saturating_sub(settle_samples);
        if usable_samples < requested.fft_size {
            return Err(js_error(&format!(
                "Post-settling dwell has {usable_samples} samples, fewer than FFT size {}",
                requested.fft_size
            )));
        }
        Ok(Self {
            plan,
            sample_rate_hz: actual_rate_hz,
            step_hz,
            lo_offset_hz,
            fixed_lo_offset: requested.fixed_lo_offset,
            dwell_samples,
            settle_samples,
            fft_size: requested.fft_size,
        })
    }

    pub(crate) fn lo_offset_for_sweep(&self, sweep_index: u64) -> f64 {
        if !self.fixed_lo_offset && sweep_index % 2 == 1 {
            -self.lo_offset_hz
        } else {
            self.lo_offset_hz
        }
    }

    pub(crate) fn worker_config(&self) -> WorkerConfig {
        WorkerConfig {
            bands: self.plan.bands.clone(),
            sample_rate_hz: self.sample_rate_hz,
            fft_size: self.fft_size,
        }
    }
}

fn validate_tune_extremes(plan: &ScanPlan, lo_offset_hz: f64, fixed: bool) -> Result<(), JsValue> {
    let first = plan.bands.first().expect("scan plan is nonempty").center_hz;
    let last = plan.bands.last().expect("scan plan is nonempty").center_hz;
    let offsets: &[f64] = if fixed {
        &[lo_offset_hz]
    } else {
        &[lo_offset_hz.abs(), -lo_offset_hz.abs()]
    };
    for center in [first, last] {
        for &offset in offsets {
            uhd_pure::b2xx::RxTuneRequest::with_lo_offset(center, offset)
                .validate()
                .map_err(display_error)?;
        }
    }
    Ok(())
}

fn duration_samples(milliseconds: f64, rate_hz: f64, label: &str) -> Result<usize, JsValue> {
    let samples = (milliseconds * rate_hz / 1000.0).ceil();
    if !samples.is_finite() || samples < 0.0 || samples > usize::MAX as f64 {
        Err(js_error(&format!("{label} sample count is out of range")))
    } else {
        Ok(samples as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested() -> RequestedSettings {
        RequestedSettings {
            start_hz: 5.15e9,
            stop_hz: 5.895e9,
            sample_rate_hz: 8e6,
            step_hz: None,
            dwell_ms: 200.0,
            settle_ms: 70.0,
            fft_size: 4096,
            gain_db: 30.0,
            lo_offset_hz: None,
            fixed_lo_offset: false,
        }
    }

    #[test]
    fn resolves_native_like_defaults() {
        let settings = ResolvedSettings::new(requested(), 8e6).unwrap();
        assert_eq!(settings.step_hz, 6.4e6);
        assert_eq!(settings.lo_offset_hz, 3.6e6);
        assert_eq!(settings.dwell_samples, 1_600_000);
        assert_eq!(settings.settle_samples, 560_000);
        assert!(settings.plan.bands.len() > 100);
    }

    #[test]
    fn lo_offset_alternates_by_complete_sweep() {
        let settings = ResolvedSettings::new(requested(), 8e6).unwrap();
        assert_eq!(settings.lo_offset_for_sweep(0), 3.6e6);
        assert_eq!(settings.lo_offset_for_sweep(1), -3.6e6);
        assert_eq!(settings.lo_offset_for_sweep(2), 3.6e6);
    }
}
