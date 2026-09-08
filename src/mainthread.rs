use std::cell::{Cell, OnceCell, RefCell};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use futures_timer::Delay;
use log::{info, warn};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, Response, Url};

use rustradio::Complex;
use rustradio_ui::TaggedVec;
use rustradio_ui::mainthread::xy_sink::{
    XyAxisFormat, XyPoint, XyRegion, XySeries, XySink, XySinkOptions,
};
use rustradio_ui::mainthread::{get_button, get_element, get_input, send_message, start_worker};

use crate::model::{ScanPlan, SurveySummary, WorkerConfig, linear_to_db};
use crate::{MainToWorker, SurveyCommand, SurveyEvent, WorkerToMain};

pub(crate) const ID_LOG_OUTPUT: &str = "log-output";

const ID_START: &str = "button-start";
const ID_STOP: &str = "button-stop";
const ID_DOWNLOAD_DATA: &str = "button-download-data";
const ID_DOWNLOAD_DB: &str = "button-download-db";
const ID_DOWNLOAD_LINEAR: &str = "button-download-linear";
const ID_STATUS: &str = "survey-status";
const ID_START_FREQUENCY: &str = "input-start-frequency";
const ID_STOP_FREQUENCY: &str = "input-stop-frequency";
const ID_SAMPLE_RATE: &str = "input-sample-rate";
const ID_STEP: &str = "input-step";
const ID_DWELL: &str = "input-dwell";
const ID_SETTLE: &str = "input-settle";
const ID_FFT_SIZE: &str = "input-fft-size";
const ID_GAIN: &str = "input-gain";
const ID_LO_OFFSET: &str = "input-lo-offset";
const ID_FIXED_LO: &str = "input-fixed-lo";
const ID_DB_PLOT: &str = "plot-db";
const ID_LINEAR_PLOT: &str = "plot-linear";

const B200_FIRMWARE_IMAGE: &str = "usrp_b200_fw.hex";
const B200_FPGA_IMAGE: &str = "usrp_b200_fpga.bin";
const B200_REENUMERATION_DELAY: Duration = Duration::from_secs(1);
const SAMPLE_BATCH_SIZE: usize = 65_536;
const MAX_DWELL_ATTEMPTS: usize = 300;
const DWELL_FAILURES_PER_COOLDOWN: usize = 10;
const OVERLOAD_COOLDOWN: Duration = Duration::from_secs(1);
const WORKER_FAILURE_ACK: u64 = u64::MAX;

thread_local! {
    static DB_PLOT: OnceCell<XySink> = const { OnceCell::new() };
    static LINEAR_PLOT: OnceCell<XySink> = const { OnceCell::new() };
    static LATEST_SUMMARY: RefCell<Option<SurveySummary>> = const { RefCell::new(None) };
    static STOP_REQUESTED: Cell<bool> = const { Cell::new(false) };
    static SWEEP_ACK: RefCell<Option<Sender<u64>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug)]
struct RequestedSettings {
    start_hz: f64,
    stop_hz: f64,
    sample_rate_hz: f64,
    step_hz: Option<f64>,
    dwell_ms: f64,
    settle_ms: f64,
    fft_size: usize,
    gain_db: f64,
    lo_offset_hz: Option<f64>,
    fixed_lo_offset: bool,
}

impl RequestedSettings {
    fn from_form() -> Result<Self, JsValue> {
        let fft = number(ID_FFT_SIZE, "FFT size")?;
        if fft.fract() != 0.0 || fft < 1.0 || fft > usize::MAX as f64 {
            return Err(js_error("FFT size must be a positive integer"));
        }
        Ok(Self {
            start_hz: number(ID_START_FREQUENCY, "start frequency")? * 1e6,
            stop_hz: number(ID_STOP_FREQUENCY, "stop frequency")? * 1e6,
            sample_rate_hz: number(ID_SAMPLE_RATE, "sample rate")? * 1e6,
            step_hz: optional_number(ID_STEP, "step")?.map(|value| value * 1e6),
            dwell_ms: number(ID_DWELL, "dwell")?,
            settle_ms: number(ID_SETTLE, "settle")?,
            fft_size: fft as usize,
            gain_db: number(ID_GAIN, "gain")?,
            lo_offset_hz: optional_number(ID_LO_OFFSET, "LO offset")?.map(|value| value * 1e6),
            fixed_lo_offset: get_input(ID_FIXED_LO)?.checked(),
        })
    }

    fn validate_before_usb(self) -> Result<Self, JsValue> {
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
struct ResolvedSettings {
    plan: ScanPlan,
    sample_rate_hz: f64,
    step_hz: f64,
    lo_offset_hz: f64,
    fixed_lo_offset: bool,
    dwell_samples: usize,
    settle_samples: usize,
    fft_size: usize,
}

impl ResolvedSettings {
    fn new(requested: RequestedSettings, actual_rate_hz: f64) -> Result<Self, JsValue> {
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

    fn lo_offset_for_sweep(&self, sweep_index: u64) -> f64 {
        if !self.fixed_lo_offset && sweep_index % 2 == 1 {
            -self.lo_offset_hz
        } else {
            self.lo_offset_hz
        }
    }

    fn worker_config(&self) -> WorkerConfig {
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

async fn worker_msg(message: WorkerToMain) -> Result<(), JsValue> {
    match message {
        WorkerToMain::LogLine { .. } => {}
        WorkerToMain::Ready(_) => {
            info!("Survey worker is ready");
            get_button(ID_START)?.set_disabled(false);
            set_status("Ready. Connect a USB 3 USRP B200 to begin.")?;
        }
        WorkerToMain::ApplicationSpecific(SurveyEvent::SweepComplete(summary)) => {
            let sweep_index = summary.completed_sweeps - 1;
            let render_result = render_summary(&summary);
            LATEST_SUMMARY.with(|slot| *slot.borrow_mut() = Some(summary));
            enable_downloads(true);
            let ack = SWEEP_ACK.with(|slot| slot.borrow().as_ref().cloned());
            if let Some(ack) = ack {
                let _ = ack.send(sweep_index).await;
            }
            render_result?;
        }
        WorkerToMain::ApplicationSpecific(SurveyEvent::Failed(reason)) => {
            warn!("Survey worker failed: {reason}");
            set_status(&format!("Worker failed: {reason}"))?;
            STOP_REQUESTED.with(|stop| stop.set(true));
            let ack = SWEEP_ACK.with(|slot| slot.borrow().as_ref().cloned());
            if let Some(ack) = ack {
                let _ = ack.send(WORKER_FAILURE_ACK).await;
            }
        }
        other => info!("Ignoring worker message: {other:?}"),
    }
    Ok(())
}

fn handle_start() -> Result<(), JsValue> {
    let settings = RequestedSettings::from_form()?.validate_before_usb()?;
    STOP_REQUESTED.with(|stop| stop.set(false));
    LATEST_SUMMARY.with(|slot| *slot.borrow_mut() = None);
    with_plot(&DB_PLOT, |plot| plot.clear())?;
    with_plot(&LINEAR_PLOT, |plot| plot.clear())?;
    enable_downloads(false);
    set_running_controls(true)?;
    set_status("Opening the WebUSB device chooser…")?;
    let (ack_tx, ack_rx) = async_channel::bounded(1);
    SWEEP_ACK.with(|slot| *slot.borrow_mut() = Some(ack_tx));

    spawn_local(async move {
        let result = run_survey(settings, ack_rx).await;
        SWEEP_ACK.with(|slot| *slot.borrow_mut() = None);
        if let Err(error) = result {
            let detail = error.as_string().unwrap_or_else(|| format!("{error:?}"));
            warn!("Survey stopped with an error: {detail}");
            let _ = set_status(&format!("Survey failed: {detail}"));
        }
        let _ = set_running_controls(false);
    });
    Ok(())
}

fn handle_stop() -> Result<(), JsValue> {
    STOP_REQUESTED.with(|stop| stop.set(true));
    get_button(ID_STOP)?.set_disabled(true);
    set_status("Stop requested; finishing the current sweep…")
}

async fn run_survey(
    requested: RequestedSettings,
    acknowledgements: Receiver<u64>,
) -> Result<(), JsValue> {
    let Some(mut receiver) = open_b200_receiver(requested).await? else {
        return Ok(());
    };
    let settings = ResolvedSettings::new(requested, receiver.sample_rate_hz())?;
    info!(
        "Survey has {} bands, actual rate {:.6} MS/s, step {:.6} MHz, LO offset {:+.6} MHz",
        settings.plan.bands.len(),
        settings.sample_rate_hz / 1e6,
        settings.step_hz / 1e6,
        settings.lo_offset_hz / 1e6,
    );
    send_message(MainToWorker::Start(settings.worker_config()))
        .await
        .map_err(display_error)?;

    let mut sweep_index = 0u64;
    loop {
        let lo_offset_hz = settings.lo_offset_for_sweep(sweep_index);
        for (band_index, band) in settings.plan.bands.iter().enumerate() {
            set_status(&format!(
                "Sweep {}: band {}/{} at {:.3} MHz (LO {:+.3} MHz)",
                sweep_index + 1,
                band_index + 1,
                settings.plan.bands.len(),
                band.center_hz / 1e6,
                lo_offset_hz / 1e6,
            ))?;
            capture_band(
                &mut receiver,
                &settings,
                band_index,
                band.center_hz,
                lo_offset_hz,
            )
            .await?;
        }

        receiver.stop().await.map_err(display_error)?;
        send_message(MainToWorker::ApplicationSpecific(
            SurveyCommand::CommitSweep { sweep_index },
        ))
        .await
        .map_err(display_error)?;
        let acknowledged = acknowledgements
            .recv()
            .await
            .map_err(|error| js_error(&format!("worker acknowledgement failed: {error}")))?;
        if acknowledged == WORKER_FAILURE_ACK {
            return Err(js_error("survey worker could not commit the sweep"));
        }
        if acknowledged != sweep_index {
            return Err(js_error(&format!(
                "worker acknowledged sweep {acknowledged}, expected {sweep_index}"
            )));
        }
        sweep_index += 1;
        if STOP_REQUESTED.with(Cell::get) {
            set_status(&format!(
                "Stopped after {sweep_index} complete sweep{}.",
                if sweep_index == 1 { "" } else { "s" }
            ))?;
            return Ok(());
        }
    }
}

async fn capture_band(
    receiver: &mut uhd_pure::b2xx::B2xxReceiver,
    settings: &ResolvedSettings,
    band_index: usize,
    center_hz: f64,
    lo_offset_hz: f64,
) -> Result<(), JsValue> {
    for attempt in 1..=MAX_DWELL_ATTEMPTS {
        receiver.stop().await.map_err(display_error)?;
        let tune = receiver
            .tune(uhd_pure::b2xx::RxTuneRequest::with_lo_offset(
                center_hz,
                lo_offset_hz,
            ))
            .await
            .map_err(display_error)?;
        info!(
            "Band {band_index}: center {:.6} MHz, RF LO {:.6} MHz, DSP {:+.6} MHz",
            tune.actual_center_frequency_hz / 1e6,
            tune.actual_rf_frequency_hz / 1e6,
            tune.actual_dsp_frequency_hz / 1e6,
        );
        send_message(MainToWorker::ApplicationSpecific(
            SurveyCommand::BeginDwell {
                band_index,
                lo_offset_hz,
            },
        ))
        .await
        .map_err(display_error)?;
        if let Err(error) = receiver.start().await {
            send_message(MainToWorker::ApplicationSpecific(
                SurveyCommand::CancelDwell,
            ))
            .await
            .map_err(display_error)?;
            return Err(display_error(error));
        }

        let mut seen = 0usize;
        // Never await worker backpressure during the receive interval. These
        // batches live in shared WASM memory and are handed to the FFT worker
        // only after the B200 stream has stopped.
        let mut batches = Vec::new();
        let mut batch = Vec::with_capacity(SAMPLE_BATCH_SIZE);
        let mut discontinuity = None;
        let mut fatal_receive_error = None;
        while seen < settings.dwell_samples {
            let packet = match receiver.receive().await {
                Ok(packet) => packet,
                Err(
                    error @ (uhd_pure::Error::ReceiveOverflow { .. }
                    | uhd_pure::Error::DeviceReceiveOverflow { .. }),
                ) => {
                    discontinuity = Some(error.to_string());
                    break;
                }
                Err(error) => {
                    fatal_receive_error = Some(error);
                    break;
                }
            };
            let available = (settings.dwell_samples - seen).min(packet.samples.len());
            let packet_start = seen;
            let packet_end = seen + available;
            let keep_start = settings.settle_samples.clamp(packet_start, packet_end) - packet_start;
            let keep_end = available;
            if keep_end > keep_start {
                batch.extend(
                    packet.samples[keep_start..keep_end]
                        .iter()
                        .map(|sample| Complex::new(sample.re, sample.im)),
                );
                if batch.len() >= SAMPLE_BATCH_SIZE {
                    batches.push(std::mem::take(&mut batch));
                    batch = Vec::with_capacity(SAMPLE_BATCH_SIZE);
                }
            }
            seen = packet_end;
        }
        // Do this before sending samples to the FFT worker. In particular,
        // receive() restarts the stream after a device FIFO overflow.
        receiver.stop().await.map_err(display_error)?;

        if let Some(error) = fatal_receive_error {
            send_message(MainToWorker::ApplicationSpecific(
                SurveyCommand::CancelDwell,
            ))
            .await
            .map_err(display_error)?;
            return Err(display_error(error));
        }
        if let Some(reason) = discontinuity {
            batches.clear();
            batch.clear();
            send_message(MainToWorker::ApplicationSpecific(
                SurveyCommand::CancelDwell,
            ))
            .await
            .map_err(display_error)?;
            warn!(
                "Band {band_index} receive discontinuity on attempt {attempt}/{MAX_DWELL_ATTEMPTS}: {reason}"
            );
            if should_cool_down(attempt) && attempt < MAX_DWELL_ATTEMPTS {
                warn!(
                    "Pausing reception for {} second after {attempt} consecutive failed dwells",
                    OVERLOAD_COOLDOWN.as_secs()
                );
                set_status(&format!(
                    "Band {} paused for {} second after {attempt} consecutive receive failures…",
                    band_index + 1,
                    OVERLOAD_COOLDOWN.as_secs(),
                ))?;
                Delay::new(OVERLOAD_COOLDOWN).await;
                set_status(&format!(
                    "Retrying sweep band {} after overload cooldown…",
                    band_index + 1
                ))?;
            }
            continue;
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        for batch in batches {
            send_sample_batch(batch).await?;
        }
        send_message(MainToWorker::ApplicationSpecific(SurveyCommand::EndDwell))
            .await
            .map_err(display_error)?;
        return Ok(());
    }
    Err(js_error(&format!(
        "band {band_index} failed after {MAX_DWELL_ATTEMPTS} receive attempts"
    )))
}

fn should_cool_down(consecutive_failures: usize) -> bool {
    consecutive_failures > 0 && consecutive_failures.is_multiple_of(DWELL_FAILURES_PER_COOLDOWN)
}

async fn send_sample_batch(samples: Vec<Complex>) -> Result<(), JsValue> {
    send_message(MainToWorker::Complexes(
        crate::worker::SAMPLE_STREAM.into(),
        vec![TaggedVec {
            data: samples,
            tags: Vec::new(),
        }],
    ))
    .await
    .map_err(display_error)
}

async fn open_b200_receiver(
    requested: RequestedSettings,
) -> Result<Option<uhd_pure::b2xx::B2xxReceiver>, JsValue> {
    let info = uhd_pure::b2xx::request_device()
        .await
        .map_err(display_error)?
        .ok_or_else(|| js_error("USRP chooser was cancelled"))?;
    if !info.firmware_loaded {
        set_status("Loading USRP B200 FX3 firmware…")?;
        let firmware = fetch_image(B200_FIRMWARE_IMAGE).await?;
        let device = info.open().await.map_err(display_error)?;
        device
            .load_firmware(&firmware)
            .await
            .map_err(display_error)?;
        drop(device);
        Delay::new(B200_REENUMERATION_DELAY).await;
        set_status("Firmware loaded. Click Start again and select the re-enumerated B200.")?;
        return Ok(None);
    }

    let device = info.open().await.map_err(display_error)?;
    match device.check_firmware_compatibility().await {
        Ok(_) => {}
        Err(error @ uhd_pure::Error::FirmwareCompatibility { .. }) => {
            warn!("{error}; resetting the B200 into its FX3 bootloader");
            device.reset_fx3().await.map_err(display_error)?;
            drop(device);
            Delay::new(B200_REENUMERATION_DELAY).await;
            set_status(
                "Firmware is incompatible. Click Start again and select the B200 bootloader.",
            )?;
            return Ok(None);
        }
        Err(error) => return Err(display_error(error)),
    }
    if device.usb_speed().await.map_err(display_error)? != uhd_pure::b2xx::UsbSpeed::SuperSpeed {
        return Err(js_error(
            "The RF survey requires a USB 3 (SuperSpeed) B200 connection",
        ));
    }
    let identity = device.identity().await.map_err(display_error)?;
    if identity.product != Some(uhd_pure::b2xx::Product::B200) || identity.revision < 5 {
        return Err(js_error(&format!(
            "Unsupported USRP: product {:?}, revision {}; need B200 revision 5 or newer",
            identity.product, identity.revision
        )));
    }
    info!(
        "USRP B200 serial={} name={:?}",
        identity.serial, identity.name
    );

    set_status("Loading the USRP B200 FPGA image…")?;
    let fpga = fetch_image(B200_FPGA_IMAGE).await?;
    match device
        .load_fpga(&fpga, false)
        .await
        .map_err(display_error)?
    {
        uhd_pure::b2xx::LoadOutcome::AlreadyLoaded => info!("B200 FPGA image already loaded"),
        uhd_pure::b2xx::LoadOutcome::Loaded => info!("Loaded B200 FPGA image"),
    }
    set_status("Initializing the B200 radio…")?;
    let initial_center = (requested.start_hz + requested.stop_hz) / 2.0;
    let receiver = uhd_pure::b2xx::B2xxReceiver::open(
        device,
        uhd_pure::b2xx::RxConfig {
            center_frequency_hz: initial_center,
            sample_rate_hz: requested.sample_rate_hz,
            gain: uhd_pure::b2xx::RxGain::Manual(requested.gain_db),
        },
    )
    .await
    .map_err(display_error)?;
    Ok(Some(receiver))
}

async fn fetch_image(filename: &str) -> Result<Vec<u8>, JsValue> {
    let window = web_sys::window().ok_or_else(|| js_error("no browser window"))?;
    let response = JsFuture::from(window.fetch_with_str(filename))
        .await?
        .dyn_into::<Response>()?;
    if !response.ok() {
        return Err(js_error(&format!(
            "Could not download {filename}: HTTP {}; place the UHD image next to the application WASM",
            response.status()
        )));
    }
    let buffer = JsFuture::from(response.array_buffer()?).await?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn render_summary(summary: &SurveySummary) -> Result<(), JsValue> {
    let regions = wifi_regions();
    let average_db = summary
        .points
        .iter()
        .map(|point| XyPoint::new(point.frequency_hz, linear_to_db(point.average_power)))
        .collect();
    let maximum_db = summary
        .points
        .iter()
        .map(|point| XyPoint::new(point.frequency_hz, linear_to_db(point.maximum_power)))
        .collect();
    with_plot(&DB_PLOT, |plot| {
        plot.update(
            vec![
                XySeries {
                    label: "Average".into(),
                    color: "#1558d6".into(),
                    points: average_db,
                },
                XySeries {
                    label: "Maximum".into(),
                    color: "#e02b2b".into(),
                    points: maximum_db,
                },
            ],
            regions.clone(),
        )
    })?;

    let average_linear = summary
        .points
        .iter()
        .map(|point| XyPoint::new(point.frequency_hz, point.average_power))
        .collect();
    let maximum_linear = summary
        .points
        .iter()
        .map(|point| XyPoint::new(point.frequency_hz, point.maximum_power))
        .collect();
    with_plot(&LINEAR_PLOT, |plot| {
        plot.update(
            vec![
                XySeries {
                    label: "Average".into(),
                    color: "#1558d6".into(),
                    points: average_linear,
                },
                XySeries {
                    label: "Maximum".into(),
                    color: "#e02b2b".into(),
                    points: maximum_linear,
                },
            ],
            regions,
        )
    })?;
    set_status(&format!(
        "Rendered {} bins after {} complete sweep{}.",
        summary.points.len(),
        summary.completed_sweeps,
        if summary.completed_sweeps == 1 {
            ""
        } else {
            "s"
        }
    ))
}

fn wifi_regions() -> Vec<XyRegion> {
    const BLUE: &str = "rgba(115, 125, 245, 0.20)";
    const RED: &str = "rgba(245, 115, 115, 0.20)";
    const GREEN: &str = "rgba(105, 220, 115, 0.20)";
    let mut regions = vec![
        XyRegion {
            x_min: 2.401e9,
            x_max: 2.423e9,
            label: "Channel 1".into(),
            fill_color: RED.into(),
        },
        XyRegion {
            x_min: 2.426e9,
            x_max: 2.448e9,
            label: "Channel 6".into(),
            fill_color: GREEN.into(),
        },
        XyRegion {
            x_min: 2.451e9,
            x_max: 2.473e9,
            label: "Channel 11".into(),
            fill_color: BLUE.into(),
        },
    ];
    let channels = [
        32_u16, 36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136,
        140, 144, 149, 153, 157, 161, 165, 169, 173, 177,
    ];
    for (index, channel) in channels.into_iter().enumerate() {
        let center_mhz = 5000.0 + f64::from(channel) * 5.0;
        regions.push(XyRegion {
            x_min: (center_mhz - 10.0) * 1e6,
            x_max: (center_mhz + 10.0) * 1e6,
            label: channel.to_string(),
            fill_color: [BLUE, RED, GREEN][index % 3].into(),
        });
    }
    regions
}

fn summary_text(summary: &SurveySummary) -> String {
    let mut output = String::from(
        "# frequency_hz average_power_dbfs_per_hz maximum_power_dbfs_per_hz observations\n",
    );
    for point in &summary.points {
        output.push_str(&format!(
            "{:.6} {:.9} {:.9} {}\n",
            point.frequency_hz,
            linear_to_db(point.average_power),
            linear_to_db(point.maximum_power),
            point.observations
        ));
    }
    output
}

fn handle_download_data() -> Result<(), JsValue> {
    let text = LATEST_SUMMARY.with(|slot| slot.borrow().as_ref().map(summary_text));
    let text = text.ok_or_else(|| js_error("no completed sweep to download"))?;
    let parts = js_sys::Array::new();
    parts.push(&JsValue::from_str(&text));
    let options = BlobPropertyBag::new();
    options.set_type("text/plain;charset=utf-8");
    let blob = Blob::new_with_str_sequence_and_options(&parts, &options)?;
    let url = Url::create_object_url_with_blob(&blob)?;
    let result = trigger_download(&url, "rf-survey-summary.txt");
    Url::revoke_object_url(&url)?;
    result
}

fn handle_download_plot(
    plot: &'static std::thread::LocalKey<OnceCell<XySink>>,
    filename: &str,
) -> Result<(), JsValue> {
    let url = plot.with(|slot| {
        slot.get()
            .ok_or_else(|| js_error("plot has not been initialized"))?
            .png_data_url()
            .map_err(display_error)
    })?;
    trigger_download(&url, filename)
}

fn trigger_download(url: &str, filename: &str) -> Result<(), JsValue> {
    let document = web_sys::window()
        .ok_or_else(|| js_error("no browser window"))?
        .document()
        .ok_or_else(|| js_error("no browser document"))?;
    let anchor = document
        .create_element("a")?
        .dyn_into::<HtmlAnchorElement>()?;
    anchor.set_href(url);
    anchor.set_download(filename);
    let body = document
        .body()
        .ok_or_else(|| js_error("document has no body"))?;
    body.append_child(&anchor)?;
    anchor.click();
    body.remove_child(&anchor)?;
    Ok(())
}

fn number(id: &str, label: &str) -> Result<f64, JsValue> {
    let value = get_input(id)?.value();
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|error| js_error(&format!("Invalid {label}: {error}")))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(js_error(&format!("{label} must be finite")))
    }
}

fn optional_number(id: &str, label: &str) -> Result<Option<f64>, JsValue> {
    let value = get_input(id)?.value();
    if value.trim().is_empty() {
        Ok(None)
    } else {
        number(id, label).map(Some)
    }
}

fn duration_samples(milliseconds: f64, rate_hz: f64, label: &str) -> Result<usize, JsValue> {
    let samples = (milliseconds * rate_hz / 1000.0).ceil();
    if !samples.is_finite() || samples < 0.0 || samples > usize::MAX as f64 {
        Err(js_error(&format!("{label} sample count is out of range")))
    } else {
        Ok(samples as usize)
    }
}

fn set_status(message: &str) -> Result<(), JsValue> {
    get_element(ID_STATUS)?.set_text_content(Some(message));
    Ok(())
}

fn set_running_controls(running: bool) -> Result<(), JsValue> {
    get_button(ID_START)?.set_disabled(running);
    get_button(ID_STOP)?.set_disabled(!running);
    for id in [
        ID_START_FREQUENCY,
        ID_STOP_FREQUENCY,
        ID_SAMPLE_RATE,
        ID_STEP,
        ID_DWELL,
        ID_SETTLE,
        ID_FFT_SIZE,
        ID_GAIN,
        ID_LO_OFFSET,
        ID_FIXED_LO,
    ] {
        get_input(id)?.set_disabled(running);
    }
    Ok(())
}

fn enable_downloads(enabled: bool) {
    for id in [ID_DOWNLOAD_DATA, ID_DOWNLOAD_DB, ID_DOWNLOAD_LINEAR] {
        if let Ok(button) = get_button(id) {
            button.set_disabled(!enabled);
        }
    }
}

fn with_plot<T>(
    key: &'static std::thread::LocalKey<OnceCell<XySink>>,
    operation: impl FnOnce(&XySink) -> rustradio::Result<T>,
) -> Result<T, JsValue> {
    key.with(|slot| {
        operation(
            slot.get()
                .ok_or_else(|| js_error("plot has not been initialized"))?,
        )
        .map_err(display_error)
    })
}

fn display_error(error: impl std::fmt::Display) -> JsValue {
    js_error(&error.to_string())
}

fn js_error(message: &str) -> JsValue {
    JsValue::from_str(message)
}

pub(crate) async fn setup() -> Result<(), JsValue> {
    {
        let handler = Closure::<dyn FnMut() -> Result<(), JsValue>>::new(handle_start);
        get_button(ID_START)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }
    {
        let handler = Closure::<dyn FnMut() -> Result<(), JsValue>>::new(handle_stop);
        get_button(ID_STOP)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }
    {
        let handler = Closure::<dyn FnMut() -> Result<(), JsValue>>::new(handle_download_data);
        get_button(ID_DOWNLOAD_DATA)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }
    {
        let handler = Closure::<dyn FnMut() -> Result<(), JsValue>>::new(|| {
            handle_download_plot(&DB_PLOT, "rf-survey-db.png")
        });
        get_button(ID_DOWNLOAD_DB)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }
    {
        let handler = Closure::<dyn FnMut() -> Result<(), JsValue>>::new(|| {
            handle_download_plot(&LINEAR_PLOT, "rf-survey-power.png")
        });
        get_button(ID_DOWNLOAD_LINEAR)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }

    let db_plot = XySink::mount_by_id(
        ID_DB_PLOT,
        XySinkOptions {
            title: "Signal strength (dB)".into(),
            subtitle: "Cumulative average and maximum after complete sweeps".into(),
            x_label: "Frequency".into(),
            y_label: "dBFS/Hz".into(),
            x_format: XyAxisFormat::FrequencyHz,
            y_format: XyAxisFormat::Decimal,
            include_y_zero: false,
        },
    )
    .map_err(display_error)?;
    let _ = DB_PLOT.with(|slot| slot.set(db_plot));
    let linear_plot = XySink::mount_by_id(
        ID_LINEAR_PLOT,
        XySinkOptions {
            title: "Signal strength (linear power)".into(),
            subtitle: "The same cumulative data on a linear scale".into(),
            x_label: "Frequency".into(),
            y_label: "FS²/Hz".into(),
            x_format: XyAxisFormat::FrequencyHz,
            y_format: XyAxisFormat::Scientific,
            include_y_zero: true,
        },
    )
    .map_err(display_error)?;
    let _ = LINEAR_PLOT.with(|slot| slot.set(linear_plot));

    enable_downloads(false);
    let _worker =
        start_worker::<crate::MainApplication, crate::WorkerApplication, _, _>(worker_msg);
    Ok(())
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

    #[test]
    fn summary_export_matches_native_columns() {
        let text = summary_text(&SurveySummary {
            completed_sweeps: 1,
            points: vec![crate::model::SummaryPoint {
                frequency_hz: 100e6,
                average_power: 1e-10,
                maximum_power: 1e-9,
                observations: 1,
            }],
        });
        assert_eq!(
            text,
            "# frequency_hz average_power_dbfs_per_hz maximum_power_dbfs_per_hz observations\n100000000.000000 -100.000000000 -90.000000000 1\n"
        );
    }

    #[test]
    fn overload_cooldown_repeats_every_ten_failed_dwells() {
        for failures in 1..DWELL_FAILURES_PER_COOLDOWN {
            assert!(!should_cool_down(failures));
        }
        assert!(should_cool_down(DWELL_FAILURES_PER_COOLDOWN));
        assert!(!should_cool_down(DWELL_FAILURES_PER_COOLDOWN + 1));
        assert!(should_cool_down(DWELL_FAILURES_PER_COOLDOWN * 2));
    }
}
