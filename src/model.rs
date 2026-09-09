use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ScanBand {
    pub center_hz: f64,
    pub low_hz: f64,
    pub high_hz: f64,
    pub includes_high_edge: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScanPlan {
    pub bands: Vec<ScanBand>,
}

impl ScanPlan {
    pub fn new(
        start_hz: f64,
        stop_hz: f64,
        step_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<Self, String> {
        require(
            start_hz.is_finite() && start_hz > 0.0,
            "start frequency must be positive and finite",
        )?;
        require(
            stop_hz.is_finite() && stop_hz > start_hz,
            "stop frequency must be greater than start frequency",
        )?;
        require(
            step_hz.is_finite() && step_hz > 0.0,
            "step must be positive and finite",
        )?;
        require(
            sample_rate_hz.is_finite() && sample_rate_hz > 0.0,
            "sample rate must be positive and finite",
        )?;
        require(
            step_hz <= sample_rate_hz,
            "step must not exceed the actual sample rate",
        )?;

        let width = stop_hz - start_hz;
        let band_count_f64 = (width / step_hz).ceil().max(1.0);
        require(
            band_count_f64 <= usize::MAX as f64,
            "frequency range requires too many tuner centers",
        )?;
        let band_count = band_count_f64 as usize;
        let band_width = width / band_count as f64;
        let bands = (0..band_count)
            .map(|index| {
                let low_hz = start_hz + index as f64 * band_width;
                let includes_high_edge = index + 1 == band_count;
                let high_hz = if includes_high_edge {
                    stop_hz
                } else {
                    start_hz + (index + 1) as f64 * band_width
                };
                ScanBand {
                    center_hz: (low_hz + high_hz) / 2.0,
                    low_hz,
                    high_hz,
                    includes_high_edge,
                }
            })
            .collect();
        Ok(Self { bands })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct WorkerConfig {
    pub bands: Vec<ScanBand>,
    pub sample_rate_hz: f64,
    pub fft_size: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SummaryPoint {
    pub frequency_hz: f64,
    pub average_power: f64,
    pub maximum_power: f64,
    pub observations: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PlotData {
    pub completed_sweeps: u64,
    pub bins: usize,
    pub db: rustradio_ui::mainthread::xy_sink::XyEnvelope,
    pub linear: rustradio_ui::mainthread::xy_sink::XyEnvelope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DeviceKey {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
}

#[cfg(test)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SurveySummary {
    pub completed_sweeps: u64,
    pub points: Vec<SummaryPoint>,
}

pub(crate) fn shifted_indices(fft_size: usize) -> impl Iterator<Item = usize> {
    let first_negative = fft_size.div_ceil(2);
    (first_negative..fft_size).chain(0..first_negative)
}

pub(crate) fn bin_offset(bin: usize, fft_size: usize, sample_rate_hz: f64) -> f64 {
    let first_negative = fft_size.div_ceil(2);
    let signed_bin = if bin >= first_negative {
        bin as isize - fft_size as isize
    } else {
        bin as isize
    };
    signed_bin as f64 * sample_rate_hz / fft_size as f64
}

pub(crate) fn linear_to_db(power: f64) -> f64 {
    if power == 0.0 {
        f64::NEG_INFINITY
    } else {
        10.0 * power.log10()
    }
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_plan_covers_range_with_equal_bands() {
        let plan = ScanPlan::new(100.0, 110.0, 4.0, 5.0).unwrap();
        assert_eq!(plan.bands.len(), 3);
        assert_eq!(plan.bands.first().unwrap().low_hz, 100.0);
        assert_eq!(plan.bands.last().unwrap().high_hz, 110.0);
        assert!(plan.bands.last().unwrap().includes_high_edge);
        let widths: Vec<_> = plan
            .bands
            .iter()
            .map(|band| band.high_hz - band.low_hz)
            .collect();
        assert!(
            widths
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() < 1e-12)
        );
    }

    #[test]
    fn shifted_bins_are_in_frequency_order() {
        let bins: Vec<_> = shifted_indices(4).collect();
        assert_eq!(bins, vec![2, 3, 0, 1]);
        let offsets: Vec<_> = bins
            .into_iter()
            .map(|bin| bin_offset(bin, 4, 4.0))
            .collect();
        assert_eq!(offsets, vec![-2.0, -1.0, 0.0, 1.0]);
    }
}
