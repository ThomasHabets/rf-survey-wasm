use std::cell::RefCell;
use std::sync::Arc;

use async_channel::Receiver;
use log::{error, info, trace};
use rustfft::FftPlanner;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use rustradio::Complex;
use rustradio::window::WindowType;
use rustradio_ui::AppEmpty;

use crate::model::{SummaryPoint, SurveySummary, WorkerConfig, bin_offset, shifted_indices};
use crate::{MainToWorker, SurveyCommand, SurveyEvent, WorkerToMain};

pub(crate) const SAMPLE_STREAM: &str = "survey-samples";

thread_local! {
    static STATE: RefCell<Option<SurveyWorker>> = const { RefCell::new(None) };
}

struct SpectrumAverager {
    sample_rate_hz: f64,
    fft_size: usize,
    window: Vec<f32>,
    window_energy: f64,
    fft: Arc<dyn rustfft::Fft<f32>>,
    frame: Vec<Complex>,
    work: Vec<Complex>,
    power_sum: Vec<f64>,
    frames: u64,
}

impl SpectrumAverager {
    fn new(sample_rate_hz: f64, fft_size: usize) -> Result<Self, String> {
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
        Ok(Self {
            sample_rate_hz,
            fft_size,
            window,
            window_energy,
            fft: FftPlanner::new().plan_fft_forward(fft_size),
            frame: Vec::with_capacity(fft_size),
            work: vec![Complex::default(); fft_size],
            power_sum: vec![0.0; fft_size],
            frames: 0,
        })
    }

    fn push_all(&mut self, samples: impl IntoIterator<Item = Complex>) {
        for sample in samples {
            self.push(sample);
        }
    }

    fn push(&mut self, sample: Complex) {
        self.frame.push(sample);
        if self.frame.len() != self.fft_size {
            return;
        }
        if self
            .frame
            .iter()
            .all(|sample| *sample == Complex::default())
        {
            self.frame.clear();
            return;
        }
        for ((destination, source), window) in
            self.work.iter_mut().zip(&self.frame).zip(&self.window)
        {
            *destination = *source * *window;
        }
        self.fft.process(&mut self.work);
        for (sum, bin) in self.power_sum.iter_mut().zip(&self.work) {
            *sum += f64::from(bin.norm_sqr());
        }
        self.frames += 1;
        self.frame.clear();
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
    fn new(config: &WorkerConfig) -> Self {
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

struct SurveyWorker {
    config: WorkerConfig,
    averager: SpectrumAverager,
    active: Option<ActiveDwell>,
    staged: Vec<Option<Measurement>>,
    summary: RunSummary,
}

impl SurveyWorker {
    fn new(config: WorkerConfig) -> Result<Self, String> {
        if config.bands.is_empty() {
            return Err("survey has no tuner bands".into());
        }
        let averager = SpectrumAverager::new(config.sample_rate_hz, config.fft_size)?;
        let summary = RunSummary::new(&config);
        let staged = std::iter::repeat_with(|| None)
            .take(config.bands.len())
            .collect();
        Ok(Self {
            config,
            averager,
            active: None,
            staged,
            summary,
        })
    }

    fn begin_dwell(&mut self, band_index: usize, lo_offset_hz: f64) -> Result<(), String> {
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

    fn push_samples(&mut self, samples: Vec<Complex>) -> Result<(), String> {
        if self.active.is_none() {
            return Err("received samples without an active dwell".into());
        }
        self.averager.push_all(samples);
        Ok(())
    }

    fn cancel_dwell(&mut self) {
        self.active = None;
        self.averager.reset();
    }

    fn end_dwell(&mut self) -> Result<(), String> {
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

    fn commit_sweep(&mut self, sweep_index: u64) -> Result<SurveySummary, String> {
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
        Ok(self.summary.snapshot(sweep_index + 1))
    }
}

async fn worker_msg(message: MainToWorker) -> Result<(), String> {
    match message {
        MainToWorker::Start(config) => {
            info!(
                "Starting survey processor: {} bands at {} S/s, FFT {}",
                config.bands.len(),
                config.sample_rate_hz,
                config.fft_size
            );
            let state = SurveyWorker::new(config)?;
            STATE.with(|slot| *slot.borrow_mut() = Some(state));
        }
        MainToWorker::ApplicationSpecific(command) => {
            let summary = STATE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let state = slot
                    .as_mut()
                    .ok_or_else(|| "survey processor has not been started".to_string())?;
                match command {
                    SurveyCommand::BeginDwell {
                        band_index,
                        lo_offset_hz,
                    } => {
                        state.begin_dwell(band_index, lo_offset_hz)?;
                        Ok(None)
                    }
                    SurveyCommand::CancelDwell => {
                        state.cancel_dwell();
                        Ok(None)
                    }
                    SurveyCommand::EndDwell => {
                        state.end_dwell()?;
                        Ok(None)
                    }
                    SurveyCommand::CommitSweep { sweep_index } => {
                        state.commit_sweep(sweep_index).map(Some)
                    }
                }
            })?;
            if let Some(summary) = summary {
                rustradio_ui::worker::send_message(WorkerToMain::ApplicationSpecific(
                    SurveyEvent::SweepComplete(summary),
                ))
                .await
                .map_err(|error| error.to_string())?;
            }
        }
        MainToWorker::Complexes(name, streams) if name == SAMPLE_STREAM => {
            STATE.with(|slot| -> Result<(), String> {
                let mut slot = slot.borrow_mut();
                let state = slot
                    .as_mut()
                    .ok_or_else(|| "survey processor has not been started".to_string())?;
                for stream in streams {
                    state.push_samples(stream.data)?;
                }
                Ok(())
            })?;
        }
        other => return Err(format!("unexpected worker message: {other:?}")),
    }
    Ok(())
}

fn ready(receiver: Receiver<MainToWorker>) {
    spawn_local(async move {
        rustradio_ui::worker::send_message(WorkerToMain::Ready(AppEmpty {}))
            .await
            .expect("failed to send ready message");
        while let Ok(message) = receiver.recv().await {
            trace!("Worker received {message:?}");
            if let Err(reason) = worker_msg(message).await {
                error!("Survey worker failed: {reason}");
                let _ = rustradio_ui::worker::send_message(WorkerToMain::ApplicationSpecific(
                    SurveyEvent::Failed(reason),
                ))
                .await;
            }
        }
    });
}

pub(crate) async fn setup() -> Result<(), JsValue> {
    rustradio_ui::worker::setup::<crate::MainApplication, crate::WorkerApplication, _>(ready).await
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
        averager.push_all(vec![Complex::default(); 4]);
        assert!(averager.finish().is_none());
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
