//! Survey acquisition and DSP share a worker, but never an active receive interval.
use crate::model::{DeviceKey, linear_to_db};
use crate::settings::{RequestedSettings, ResolvedSettings};
use crate::worker::SurveyWorker;
use crate::{MainToWorker, SurveyCommand, SurveyEvent, WorkerToMain};
use async_channel::Receiver;
use futures_timer::Delay;
use log::{debug, info, warn};
use rustradio::Complex;
use std::cell::{Cell, RefCell};
use std::fmt::Write;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::Response;

type StartRequest = (DeviceKey, RequestedSettings, [usize; 2]);
thread_local! {
    static NEXT: RefCell<Option<StartRequest>> = const { RefCell::new(None) };
    static STOP: Cell<bool> = const { Cell::new(false) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static WIDTHS: Cell<[usize; 2]> = const { Cell::new([1024,1024]) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    static EXPORT: Cell<bool> = const { Cell::new(false) };
    static RENDERED: Cell<Option<u64>> = const { Cell::new(None) };
}

const B200_FIRMWARE_IMAGE: &str = "usrp_b200_fw.hex";
const B200_FPGA_IMAGE: &str = "usrp_b200_fpga.bin";
const B200_REENUMERATION_DELAY: Duration = Duration::from_secs(1);

fn js_error(s: &str) -> JsValue {
    JsValue::from_str(s)
}
fn display_error(e: impl std::fmt::Display) -> JsValue {
    js_error(&e.to_string())
}
async fn emit(event: SurveyEvent) -> Result<(), JsValue> {
    rustradio_ui::worker::send_message(WorkerToMain::ApplicationSpecific(event))
        .await
        .map_err(display_error)
}
async fn status(text: &str, retrying: bool) -> Result<(), JsValue> {
    emit(SurveyEvent::Progress {
        text: text.into(),
        retrying,
    })
    .await
}

pub(crate) fn ready(receiver: Receiver<MainToWorker>) {
    let (wake_tx, wake_rx) = async_channel::bounded(1);
    spawn_local(async move {
        let _ = rustradio_ui::worker::send_message(WorkerToMain::Ready(rustradio_ui::AppEmpty {}))
            .await;
        while let Ok(command) = receiver.recv().await {
            match command {
                MainToWorker::Start(start) => {
                    if BUSY.with(|b| b.replace(true)) {
                        continue;
                    }
                    NEXT.with(|s| *s.borrow_mut() = Some(start));
                }
                MainToWorker::ApplicationSpecific(SurveyCommand::Stop) => {
                    STOP.with(|s| s.set(true))
                }
                MainToWorker::ApplicationSpecific(SurveyCommand::Viewport(widths)) => {
                    WIDTHS.with(|w| w.set(widths.map(|n| n.clamp(1, 16384))));
                    DIRTY.with(|d| d.set(true));
                }
                MainToWorker::ApplicationSpecific(SurveyCommand::Export) => {
                    EXPORT.with(|e| e.set(true))
                }
                MainToWorker::ApplicationSpecific(SurveyCommand::Rendered(sweep)) => {
                    RENDERED.with(|r| r.set(Some(sweep)))
                }
                _ => {}
            }
            let _ = wake_tx.try_send(());
        }
    });
    spawn_local(async move {
        let mut processor = None;
        while wake_rx.recv().await.is_ok() {
            let start = NEXT.with(|s| s.borrow_mut().take());
            if let Some((key, requested, widths)) = start {
                processor = None;
                STOP.with(|s| s.set(false));
                EXPORT.with(|e| e.set(false));
                WIDTHS.with(|w| w.set(widths));
                let result = run(key, requested, &mut processor, &wake_rx).await;
                if let Err(error) = result {
                    let _ = emit(SurveyEvent::Failed(
                        error.as_string().unwrap_or_else(|| format!("{error:?}")),
                    ))
                    .await;
                }
                BUSY.with(|b| b.set(false));
                let _ = emit(SurveyEvent::Finished).await;
            }
            if let Some(processor) = processor.as_ref()
                && let Err(error) = service(processor).await
            {
                let _ = emit(SurveyEvent::Failed(format!(
                    "Result request failed: {error:?}"
                )))
                .await;
            }
        }
    });
}

async fn run(
    key: DeviceKey,
    requested: RequestedSettings,
    processor: &mut Option<SurveyWorker>,
    wake: &Receiver<()>,
) -> Result<(), JsValue> {
    let Some(mut receiver) = open_b200_receiver(key, requested).await? else {
        return Ok(());
    };
    let settings = ResolvedSettings::new(requested, receiver.sample_rate_hz())?;
    info!(
        "Survey: {} bands, {:.3} MS/s, {:.3} MHz step",
        settings.plan.bands.len(),
        settings.sample_rate_hz / 1e6,
        settings.step_hz / 1e6
    );
    *processor = Some(SurveyWorker::new(settings.worker_config()).map_err(display_error)?);
    let processor = processor.as_mut().expect("processor initialized");
    let result = sweep_loop(&mut receiver, &settings, processor, wake).await;
    let stop = receiver.stop().await.map_err(display_error);
    result.and(stop)
}

async fn sweep_loop(
    receiver: &mut uhd_pure::b2xx::B2xxReceiver,
    settings: &ResolvedSettings,
    processor: &mut SurveyWorker,
    wake: &Receiver<()>,
) -> Result<(), JsValue> {
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(settings.dwell_samples - settings.settle_samples)
        .map_err(display_error)?;
    loop {
        let sweep = processor.completed_sweeps;
        let offset = settings.lo_offset_for_sweep(sweep);
        let mut capture_ms = 0.0;
        let mut dsp_ms = 0.0;
        for (band_index, band) in settings.plan.bands.iter().enumerate() {
            service(processor).await?;
            status(
                &format!(
                    "Sweep {}: band {}/{} at {:.3} MHz",
                    sweep + 1,
                    band_index + 1,
                    settings.plan.bands.len(),
                    band.center_hz / 1e6
                ),
                false,
            )
            .await?;
            let start = js_sys::Date::now();
            if !capture(
                receiver,
                settings,
                band_index,
                band.center_hz,
                offset,
                &mut samples,
                processor,
            )
            .await?
            {
                processor.cancel_dwell();
                return status(
                    "Stopped during overload recovery; incomplete sweep discarded.",
                    false,
                )
                .await;
            }
            capture_ms += js_sys::Date::now() - start;
            let start = js_sys::Date::now();
            processor
                .begin_dwell(band_index, offset)
                .map_err(display_error)?;
            // Yield between chunks only with reception stopped, keeping control
            // messages responsive even for very long user-configured dwells.
            let mut last_yield = js_sys::Date::now();
            for chunk in samples.chunks(65_536) {
                processor.push_samples(chunk).map_err(display_error)?;
                if js_sys::Date::now() - last_yield >= 8.0 {
                    Delay::new(Duration::ZERO).await;
                    last_yield = js_sys::Date::now();
                }
            }
            processor.end_dwell().map_err(display_error)?;
            dsp_ms += js_sys::Date::now() - start;
        }
        processor.commit_sweep(sweep).map_err(display_error)?;
        let render_start = js_sys::Date::now();
        RENDERED.with(|r| r.set(None));
        emit(SurveyEvent::Plot(processor.plot(WIDTHS.with(Cell::get)))).await?;
        while RENDERED.with(Cell::get) != Some(processor.completed_sweeps) {
            wake.recv().await.map_err(display_error)?;
            service(processor).await?;
        }
        info!(
            "Sweep {}: capture/retry {:.0} ms, DSP {:.0} ms, plot/ack {:.0} ms",
            processor.completed_sweeps,
            capture_ms,
            dsp_ms,
            js_sys::Date::now() - render_start
        );
        if STOP.with(Cell::get) {
            return status(
                &format!(
                    "Stopped after {} complete sweeps.",
                    processor.completed_sweeps
                ),
                false,
            )
            .await;
        }
    }
}

fn cooldown(failures: u64) -> Option<Duration> {
    if failures == 0 || !failures.is_multiple_of(10) {
        return None;
    }
    Some(Duration::from_secs(
        (1u64 << ((failures / 10 - 1).min(4) as u32)).min(10),
    ))
}

async fn capture(
    receiver: &mut uhd_pure::b2xx::B2xxReceiver,
    settings: &ResolvedSettings,
    band: usize,
    center: f64,
    offset: f64,
    samples: &mut Vec<Complex>,
    processor: &SurveyWorker,
) -> Result<bool, JsValue> {
    receiver.stop().await.map_err(display_error)?;
    let tune = receiver
        .tune(uhd_pure::b2xx::RxTuneRequest::with_lo_offset(
            center, offset,
        ))
        .await
        .map_err(display_error)?;
    debug!(
        "Band {band}: RF {:.6} MHz, DSP {:+.6} MHz",
        tune.actual_rf_frequency_hz / 1e6,
        tune.actual_dsp_frequency_hz / 1e6
    );
    let mut failures = 0u64;
    loop {
        if failures > 0 && STOP.with(Cell::get) {
            return Ok(false);
        }
        samples.clear();
        receiver.start().await.map_err(display_error)?;
        let mut seen = 0usize;
        let mut failure = None;
        while seen < settings.dwell_samples {
            match receiver.receive_raw().await {
                Ok(packet) => {
                    let count = (settings.dwell_samples - seen).min(packet.payload.len() / 8);
                    let skip = settings.settle_samples.saturating_sub(seen).min(count);
                    append_fc32(&packet.payload[skip * 8..count * 8], packet.scale, samples);
                    seen += count;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        receiver.stop().await.map_err(display_error)?;
        let Some(error) = failure else {
            if failures > 0 {
                info!("Band {band} recovered after {failures} discarded dwells");
            }
            return Ok(true);
        };
        match error {
            uhd_pure::Error::ReceiveOverflow { .. }
            | uhd_pure::Error::DeviceReceiveOverflow { .. }
            | uhd_pure::Error::ReceiveTimeout => {}
            other => return Err(display_error(other)),
        }
        failures = failures.saturating_add(1);
        if failures == 1 {
            warn!("Band {band}: discarding discontinuous dwell: {error}");
        }
        if let Some(delay) = cooldown(failures) {
            warn!(
                "Band {band}: {failures} discarded dwells; pausing {} s. Latest: {error}",
                delay.as_secs()
            );
            status(
                &format!(
                    "Band {}: {failures} failed dwells; cooling down {} s.",
                    band + 1,
                    delay.as_secs()
                ),
                true,
            )
            .await?;
            // Keep export/viewport and stop requests responsive while paused.
            for _ in 0..delay.as_millis() / 50 {
                if STOP.with(Cell::get) {
                    return Ok(false);
                }
                service(processor).await?;
                Delay::new(Duration::from_millis(50)).await;
            }
        }
        service(processor).await?;
    }
}

fn append_fc32(payload: &[u8], scale: f32, samples: &mut Vec<Complex>) {
    samples.extend(payload.as_chunks::<8>().0.iter().map(|sample| {
        Complex::new(
            f32::from_le_bytes(sample[..4].try_into().expect("four bytes")) * scale,
            f32::from_le_bytes(sample[4..].try_into().expect("four bytes")) * scale,
        )
    }));
}

async fn service(processor: &SurveyWorker) -> Result<(), JsValue> {
    if EXPORT.with(|e| e.replace(false)) && processor.completed_sweeps > 0 {
        let mut chunk = String::from(
            "# frequency_hz average_power_dbfs_per_hz maximum_power_dbfs_per_hz observations\n",
        );
        for point in processor.points() {
            writeln!(
                chunk,
                "{:.6} {:.9} {:.9} {}",
                point.frequency_hz,
                linear_to_db(point.average_power),
                linear_to_db(point.maximum_power),
                point.observations
            )
            .expect("String formatting");
            if chunk.len() >= 65_536 {
                emit(SurveyEvent::ExportChunk(
                    std::mem::take(&mut chunk).into_bytes(),
                ))
                .await?;
                Delay::new(Duration::ZERO).await;
            }
        }
        if !chunk.is_empty() {
            emit(SurveyEvent::ExportChunk(chunk.into_bytes())).await?;
        }
        emit(SurveyEvent::ExportComplete).await?;
    }
    if DIRTY.with(|d| d.replace(false)) && processor.completed_sweeps > 0 {
        emit(SurveyEvent::Plot(processor.plot(WIDTHS.with(Cell::get)))).await?;
    }
    Ok(())
}

async fn open_b200_receiver(
    key: DeviceKey,
    requested: RequestedSettings,
) -> Result<Option<uhd_pure::b2xx::B2xxReceiver>, JsValue> {
    let mut devices = uhd_pure::b2xx::list_devices()
        .await
        .map_err(display_error)?
        .into_iter()
        .filter(|d| {
            d.vendor_id == key.vendor_id
                && d.product_id == key.product_id
                && d.serial_number == key.serial
        });
    let info = devices
        .next()
        .ok_or_else(|| js_error("Selected USRP is no longer available"))?;
    if devices.next().is_some() {
        return Err(js_error("Selected USRP identity is ambiguous"));
    }
    if !info.firmware_loaded {
        status("Loading USRP B200 FX3 firmware…", false).await?;
        let firmware = fetch_image(B200_FIRMWARE_IMAGE).await?;
        let device = info.open().await.map_err(display_error)?;
        device
            .load_firmware(&firmware)
            .await
            .map_err(display_error)?;
        drop(device);
        Delay::new(B200_REENUMERATION_DELAY).await;
        status(
            "Firmware loaded. Click Start again and select the re-enumerated B200.",
            false,
        )
        .await?;
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
            status(
                "Firmware is incompatible. Click Start again and select the B200 bootloader.",
                false,
            )
            .await?;
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

    status("Loading the USRP B200 FPGA image…", false).await?;
    let fpga = fetch_image(B200_FPGA_IMAGE).await?;
    match device
        .load_fpga(&fpga, false)
        .await
        .map_err(display_error)?
    {
        uhd_pure::b2xx::LoadOutcome::AlreadyLoaded => info!("B200 FPGA image already loaded"),
        uhd_pure::b2xx::LoadOutcome::Loaded => info!("Loaded B200 FPGA image"),
    }
    status("Initializing the B200 radio…", false).await?;
    let initial_center = (requested.start_hz + requested.stop_hz) / 2.0;
    let receiver = uhd_pure::b2xx::B2xxReceiver::open_with_options(
        device,
        uhd_pure::b2xx::RxConfig {
            center_frequency_hz: initial_center,
            sample_rate_hz: requested.sample_rate_hz,
            gain: uhd_pure::b2xx::RxGain::Manual(requested.gain_db),
        },
        uhd_pure::b2xx::RxOptions {
            queue_depth: 16,
            auto_restart_on_overflow: false,
            start_immediately: false,
        },
    )
    .await
    .map_err(display_error)?;
    Ok(Some(receiver))
}

async fn fetch_image(filename: &str) -> Result<Vec<u8>, JsValue> {
    let window = js_sys::global().unchecked_into::<web_sys::WorkerGlobalScope>();
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_is_bounded_but_never_exhausted() {
        assert_eq!(cooldown(0), None);
        assert_eq!(cooldown(9), None);
        for (n, secs) in [
            (10, 1),
            (20, 2),
            (30, 4),
            (40, 8),
            (50, 10),
            (300, 10),
            (1000000, 10),
        ] {
            assert_eq!(cooldown(n), Some(Duration::from_secs(secs)));
        }
    }
    #[test]
    fn raw_decode_preserves_normalization_and_capacity() {
        let bytes: Vec<_> = [1.0f32, -2.0, 3.0, 4.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut samples = Vec::with_capacity(2);
        let capacity = samples.capacity();
        append_fc32(&bytes, 0.5, &mut samples);
        assert_eq!(samples, [Complex::new(0.5, -1.0), Complex::new(1.5, 2.0)]);
        assert_eq!(samples.capacity(), capacity);
    }
}
