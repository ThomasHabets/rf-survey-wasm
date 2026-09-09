use std::sync::Arc;

use rustfft::FftPlanner;
use wasm_bindgen::prelude::*;

use rustradio::Complex;
use rustradio::window::WindowType;

#[cfg(test)]
use crate::model::SurveySummary;
use crate::model::{
    PlotData, SummaryPoint, WorkerConfig, bin_offset, linear_to_db, shifted_indices,
};
use rustradio_ui::mainthread::xy_sink::{XyEnvelope, XyEnvelopeSeries};

struct SpectrumAverager {
    sample_rate_hz: f64,
    fft_size: usize,
    window: Vec<f32>,
    window_energy: f64,
    fft: Arc<dyn rustfft::Fft<f32>>,
    frame: Vec<Complex>,
    work: Vec<Complex>,
    scratch: Vec<Complex>,
    power_sum: Vec<f64>,
    frames: u64,
}

impl SpectrumAverager {
    pub(crate) fn new(sample_rate_hz: f64, fft_size: usize) -> Result<Self, String> {
        if !sample_rate_hz.is_finite() || sample_rate_hz <= 0.0 {
            return Err("sample rate must be positive and finite".into());
        }
        if fft_size == 0 {
            return Err("FFT size must be greater than zero".into());
        }
        let window = WindowType::BlackmanHarris.make_window(fft_size).0;
        let window_energy = window
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>();
        if window_energy <= 0.0 {
            return Err("FFT window has zero energy".into());
        }
        let fft = FftPlanner::new().plan_fft_forward(fft_size);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        Ok(Self {
            sample_rate_hz,
            fft_size,
            window,
            window_energy,
            fft,
            scratch,
            frame: Vec::with_capacity(fft_size),
            work: vec![Complex::default(); fft_size],
            power_sum: vec![0.0; fft_size],
            frames: 0,
        })
    }

    fn push_all(&mut self, mut samples: &[Complex]) {
        if !self.frame.is_empty() {
            let count = (self.fft_size - self.frame.len()).min(samples.len());
            self.frame.extend_from_slice(&samples[..count]);
            samples = &samples[count..];
            if self.frame.len() == self.fft_size {
                let mut frame = std::mem::take(&mut self.frame);
                self.process_frame(&frame);
                frame.clear();
                self.frame = frame;
            }
        }
        let mut chunks = samples.chunks_exact(self.fft_size);
        for frame in &mut chunks {
            self.process_frame(frame);
        }
        self.frame.extend_from_slice(chunks.remainder());
    }

    fn process_frame(&mut self, frame: &[Complex]) {
        if frame.iter().all(|sample| *sample == Complex::default()) {
            return;
        }
        for ((destination, source), window) in self.work.iter_mut().zip(frame).zip(&self.window) {
            *destination = *source * *window;
        }
        self.fft
            .process_with_scratch(&mut self.work, &mut self.scratch);
        for (sum, bin) in self.power_sum.iter_mut().zip(&self.work) {
            *sum += f64::from(bin.norm_sqr());
        }
        self.frames += 1;
    }

    fn finish(&mut self) -> Option<Vec<f64>> {
        if self.frames == 0 {
            self.reset();
            return None;
        }
        let scale = self.sample_rate_hz * self.window_energy * self.frames as f64;
        let result = self.power_sum.iter().map(|power| power / scale).collect();
        self.reset();
        Some(result)
    }

    fn reset(&mut self) {
        self.frame.clear();
        self.power_sum.fill(0.0);
        self.frames = 0;
    }
}

#[derive(Clone)]
struct Measurement {
    lo_offset_hz: f64,
    psd: Vec<f64>,
}

#[derive(Clone, Copy, Default)]
struct LinearAccumulator {
    count: u64,
    total: f64,
    correction: f64,
    maximum: f64,
}

impl LinearAccumulator {
    fn add(&mut self, value: f64) {
        let adjusted = value - self.correction;
        let updated = self.total + adjusted;
        self.correction = (updated - self.total) - adjusted;
        self.total = updated;
        self.maximum = self.maximum.max(value);
        self.count += 1;
    }

    fn mean(self) -> f64 {
        self.total / self.count as f64
    }
}

struct SummaryBin {
    frequency_hz: f64,
    paths: [LinearAccumulator; 3],
}

impl SummaryBin {
    fn count(&self) -> u64 {
        self.paths.iter().map(|path| path.count).sum()
    }

    fn uses_image_rejection(&self) -> bool {
        self.paths[0].count > 0 && self.paths[2].count > 0
    }

    fn mean(&self) -> f64 {
        if self.uses_image_rejection() {
            return self.paths[0].mean().min(self.paths[2].mean());
        }
        compensated_sum(
            self.paths
                .iter()
                .filter(|path| path.count > 0)
                .map(|path| path.total),
        ) / self.count() as f64
    }

    fn maximum(&self) -> f64 {
        if self.uses_image_rejection() {
            return self.paths[0].maximum.min(self.paths[2].maximum);
        }
        self.paths
            .iter()
            .filter(|path| path.count > 0)
            .map(|path| path.maximum)
            .fold(0.0, f64::max)
    }
}

struct RunSummary {
    band_bins: Vec<Vec<(usize, usize)>>,
    bins: Vec<SummaryBin>,
}

impl RunSummary {
    pub(crate) fn new(config: &WorkerConfig) -> Self {
        let mut bins = Vec::new();
        let mut band_bins = Vec::with_capacity(config.bands.len());
        for band in &config.bands {
            let mut mappings = Vec::new();
            for fft_bin in shifted_indices(config.fft_size) {
                let frequency_hz =
                    band.center_hz + bin_offset(fft_bin, config.fft_size, config.sample_rate_hz);
                let in_band = frequency_hz >= band.low_hz
                    && (frequency_hz < band.high_hz
                        || band.includes_high_edge && frequency_hz <= band.high_hz);
                if !in_band {
                    continue;
                }
                let summary_bin = bins.len();
                bins.push(SummaryBin {
                    frequency_hz,
                    paths: [LinearAccumulator::default(); 3],
                });
                mappings.push((fft_bin, summary_bin));
            }
            band_bins.push(mappings);
        }
        Self { band_bins, bins }
    }

    fn add(&mut self, band_index: usize, measurement: &Measurement) {
        let path = if measurement.lo_offset_hz < 0.0 {
            0
        } else if measurement.lo_offset_hz > 0.0 {
            2
        } else {
            1
        };
        for &(fft_bin, summary_bin) in &self.band_bins[band_index] {
            let power = measurement.psd[fft_bin];
            if power.is_finite() && power > 0.0 {
                self.bins[summary_bin].paths[path].add(power);
            }
        }
    }

    #[cfg(test)]
    fn snapshot(&self, completed_sweeps: u64) -> SurveySummary {
        SurveySummary {
            completed_sweeps,
            points: self
                .bins
                .iter()
                .filter(|bin| bin.count() > 0)
                .map(|bin| SummaryPoint {
                    frequency_hz: bin.frequency_hz,
                    average_power: bin.mean(),
                    maximum_power: bin.maximum(),
                    observations: bin.count(),
                })
                .collect(),
        }
    }
}

fn compensated_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0;
    let mut correction = 0.0;
    for value in values {
        let adjusted = value - correction;
        let updated = total + adjusted;
        correction = (updated - total) - adjusted;
        total = updated;
    }
    total
}

struct ActiveDwell {
    band_index: usize,
    lo_offset_hz: f64,
}

pub(crate) struct SurveyWorker {
    pub(crate) completed_sweeps: u64,
    config: WorkerConfig,
    averager: SpectrumAverager,
    active: Option<ActiveDwell>,
    staged: Vec<Option<Measurement>>,
    summary: RunSummary,
}

impl SurveyWorker {
    pub(crate) fn new(config: WorkerConfig) -> Result<Self, String> {
        if config.bands.is_empty() {
            return Err("survey has no tuner bands".into());
        }
        let averager = SpectrumAverager::new(config.sample_rate_hz, config.fft_size)?;
        let summary = RunSummary::new(&config);
        let staged = std::iter::repeat_with(|| None)
            .take(config.bands.len())
            .collect();
        Ok(Self {
            completed_sweeps: 0,
            config,
            averager,
            active: None,
            staged,
            summary,
        })
    }

    pub(crate) fn begin_dwell(
        &mut self,
        band_index: usize,
        lo_offset_hz: f64,
    ) -> Result<(), String> {
        if self.active.is_some() {
            return Err("cannot begin a dwell while another dwell is active".into());
        }
        if band_index >= self.config.bands.len() {
            return Err(format!("band index {band_index} is out of range"));
        }
        if self.staged[band_index].is_some() {
            return Err(format!(
                "band {band_index} already has a staged measurement"
            ));
        }
        if !lo_offset_hz.is_finite() {
            return Err("LO offset must be finite".into());
        }
        self.averager.reset();
        self.active = Some(ActiveDwell {
            band_index,
            lo_offset_hz,
        });
        Ok(())
    }

    pub(crate) fn push_samples(&mut self, samples: &[Complex]) -> Result<(), String> {
        if self.active.is_none() {
            return Err("received samples without an active dwell".into());
        }
        self.averager.push_all(samples);
        Ok(())
    }

    pub(crate) fn cancel_dwell(&mut self) {
        self.active = None;
        self.averager.reset();
    }

    pub(crate) fn end_dwell(&mut self) -> Result<(), String> {
        let active = self
            .active
            .take()
            .ok_or_else(|| "cannot end a dwell when none is active".to_string())?;
        let psd = self.averager.finish().ok_or_else(|| {
            format!(
                "band {} produced no complete nonzero FFT frame",
                active.band_index
            )
        })?;
        self.staged[active.band_index] = Some(Measurement {
            lo_offset_hz: active.lo_offset_hz,
            psd,
        });
        Ok(())
    }

    pub(crate) fn commit_sweep(&mut self, sweep_index: u64) -> Result<(), String> {
        if self.active.is_some() {
            return Err("cannot commit a sweep while a dwell is active".into());
        }
        if let Some(missing) = self.staged.iter().position(Option::is_none) {
            return Err(format!("cannot commit sweep: band {missing} is missing"));
        }
        for (band_index, measurement) in self.staged.iter_mut().enumerate() {
            self.summary.add(
                band_index,
                &measurement.take().expect("all staged measurements checked"),
            );
        }
        self.completed_sweeps = sweep_index + 1;
        Ok(())
    }
}

pub(crate) async fn setup() -> Result<(), JsValue> {
    rustradio_ui::worker::setup::<crate::MainApplication, crate::WorkerApplication, _>(
        crate::acquisition::ready,
    )
    .await
}

impl SurveyWorker {
    pub(crate) fn points(&self) -> impl Iterator<Item = SummaryPoint> + '_ {
        self.summary
            .bins
            .iter()
            .filter(|bin| bin.count() > 0)
            .map(|bin| SummaryPoint {
                frequency_hz: bin.frequency_hz,
                average_power: bin.mean(),
                maximum_power: bin.maximum(),
                observations: bin.count(),
            })
    }

    pub(crate) fn plot(&self, widths: [usize; 2]) -> PlotData {
        let first = self.summary.bins.iter().find(|bin| bin.count() > 0);
        let last = self.summary.bins.iter().rfind(|bin| bin.count() > 0);
        let range = first
            .zip(last)
            .map_or((0.0, 1.0), |(a, b)| (a.frequency_hz, b.frequency_hz));
        let make = |count: usize| XyEnvelope {
            x_range: range,
            series: [("Average", "#1558d6"), ("Maximum", "#e02b2b")]
                .into_iter()
                .map(|(label, color)| XyEnvelopeSeries {
                    label: label.into(),
                    color: color.into(),
                    buckets: vec![None; count.clamp(1, 16384)],
                })
                .collect(),
        };
        let mut db = make(widths[0]);
        let mut linear = make(widths[1]);
        let mut bins = 0;
        for point in self.points() {
            bins += 1;
            for envelope in [&mut db, &mut linear] {
                let width = (range.1 - range.0).max(f64::EPSILON);
                let count = envelope.series[0].buckets.len();
                let index = (((point.frequency_hz - range.0) / width * count as f64) as usize)
                    .min(count - 1);
                for (series, value) in envelope
                    .series
                    .iter_mut()
                    .zip([point.average_power, point.maximum_power])
                {
                    series.buckets[index] = Some(match series.buckets[index] {
                        None => (value, value),
                        Some((lo, hi)) => (lo.min(value), hi.max(value)),
                    });
                }
            }
        }
        for series in &mut db.series {
            for (lo, hi) in series.buckets.iter_mut().flatten() {
                *lo = linear_to_db(*lo);
                *hi = linear_to_db(*hi);
            }
        }
        PlotData {
            completed_sweeps: self.completed_sweeps,
            bins,
            db,
            linear,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ScanBand;

    fn test_config() -> WorkerConfig {
        WorkerConfig {
            bands: vec![ScanBand {
                center_hz: 100.0,
                low_hz: 98.0,
                high_hz: 102.0,
                includes_high_edge: true,
            }],
            sample_rate_hz: 4.0,
            fft_size: 4,
        }
    }

    #[test]
    fn zero_frames_are_not_measurements() {
        let mut averager = SpectrumAverager::new(4.0, 4).unwrap();
        averager.push_all(&[Complex::default(); 4]);
        assert!(averager.finish().is_none());
    }

    #[test]
    fn chunked_averaging_matches_direct_dft() {
        let n = 8;
        let mut input: Vec<_> = (0..n * 2)
            .map(|i| Complex::new((i as f32 * 0.7).sin(), (i as f32 * 0.3).cos()))
            .collect();
        input.extend(vec![Complex::default(); n]);
        input.extend([Complex::new(100.0, 100.0); 3]); // incomplete tail is discarded
        let mut averager = SpectrumAverager::new(32.0, n).unwrap();
        let mut expected = vec![0.0; n];
        for frame in input[..n * 2].chunks_exact(n) {
            for (k, power) in expected.iter_mut().enumerate() {
                let mut sum = num_complex_reference(frame, &averager.window, k);
                sum /= 32.0 * averager.window_energy * 2.0;
                *power += sum;
            }
        }
        for chunk in input.chunks(3) {
            averager.push_all(chunk);
        }
        let result = averager.finish().unwrap();
        for (actual, expected) in result.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6 * expected.abs().max(1.0));
        }
        assert!(averager.finish().is_none());
    }

    fn num_complex_reference(frame: &[Complex], window: &[f32], k: usize) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (j, (sample, w)) in frame.iter().zip(window).enumerate() {
            let phase = -2.0 * std::f64::consts::PI * k as f64 * j as f64 / frame.len() as f64;
            let a = f64::from(sample.re * w);
            let b = f64::from(sample.im * w);
            re += a * phase.cos() - b * phase.sin();
            im += a * phase.sin() + b * phase.cos();
        }
        re * re + im * im
    }

    #[test]
    fn display_envelope_preserves_peaks_and_uses_bounded_columns() {
        let mut worker = SurveyWorker::new(test_config()).unwrap();
        worker.summary.add(
            0,
            &Measurement {
                lo_offset_hz: 1.0,
                psd: vec![1.0, 1000.0, 2.0, 3.0],
            },
        );
        worker.completed_sweeps = 1;
        let plot = worker.plot([2, 3]);
        assert_eq!(plot.bins, 4);
        assert_eq!(plot.db.series[0].buckets.len(), 2);
        assert_eq!(plot.linear.series[0].buckets.len(), 3);
        assert_eq!(
            plot.linear.series[1]
                .buckets
                .iter()
                .flatten()
                .map(|v| v.1)
                .fold(0.0, f64::max),
            1000.0
        );
        assert_eq!(
            plot.db.series[1]
                .buckets
                .iter()
                .flatten()
                .map(|v| v.1)
                .fold(0.0, f64::max),
            30.0
        );
        assert_eq!(worker.points().count(), 4);
    }

    #[test]
    fn summary_rejects_images_after_both_lo_paths() {
        let config = test_config();
        let mut summary = RunSummary::new(&config);
        summary.add(
            0,
            &Measurement {
                lo_offset_hz: 1.0,
                psd: vec![10.0, 20.0, 30.0, 40.0],
            },
        );
        assert_eq!(summary.snapshot(1).points[0].average_power, 30.0);
        summary.add(
            0,
            &Measurement {
                lo_offset_hz: -1.0,
                psd: vec![5.0, 10.0, 15.0, 20.0],
            },
        );
        let result = summary.snapshot(2);
        assert_eq!(result.points[0].average_power, 15.0);
        assert_eq!(result.points[0].maximum_power, 15.0);
        assert_eq!(result.points[0].observations, 2);
    }

    #[test]
    fn incomplete_sweep_cannot_be_committed() {
        let mut worker = SurveyWorker::new(test_config()).unwrap();
        assert!(worker.commit_sweep(0).is_err());
    }
}
