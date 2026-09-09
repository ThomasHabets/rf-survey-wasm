use crate::model::{DeviceKey, PlotData};
use crate::settings::RequestedSettings;
use crate::{MainToWorker, SurveyCommand, SurveyEvent, WorkerToMain};
use futures_timer::Delay;
use log::{info, warn};
use rustradio_ui::mainthread::xy_sink::{XyAxisFormat, XyRegion, XySink, XySinkOptions};
use rustradio_ui::mainthread::{get_button, get_element, get_input, send_message, start_worker};
use std::cell::{Cell, OnceCell, RefCell};
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Blob, BlobPropertyBag, Event, HtmlAnchorElement, Url};

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
const ID_THEME_TOGGLE: &str = "button-theme-toggle";
const ID_DB_PLOT: &str = "plot-db";
const ID_LINEAR_PLOT: &str = "plot-linear";

thread_local! {
    static DB_PLOT: OnceCell<XySink> = const { OnceCell::new() };
    static LINEAR_PLOT: OnceCell<XySink> = const { OnceCell::new() };
    static RUNNING: Cell<bool> = const { Cell::new(false) };
    static LATEST_SWEEP: Cell<u64> = const { Cell::new(0) };
    static EXPORT_PARTS: RefCell<Option<js_sys::Array>> = const { RefCell::new(None) };
    static RESIZE_GENERATION: Cell<u64> = const { Cell::new(0) };
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
}

fn command(command: SurveyCommand) {
    spawn_local(async move {
        if let Err(error) = send_message(MainToWorker::ApplicationSpecific(command)).await {
            warn!("Worker command failed: {error}");
        }
    });
}

fn plot_widths() -> Result<[usize; 2], JsValue> {
    Ok([
        with_plot(&DB_PLOT, |p| Ok(p.bucket_count()))?,
        with_plot(&LINEAR_PLOT, |p| Ok(p.bucket_count()))?,
    ])
}

async fn worker_msg(message: WorkerToMain) -> Result<(), JsValue> {
    match message {
        WorkerToMain::Ready(_) => {
            info!("Survey worker is ready");
            get_button(ID_START)?.set_disabled(false);
            set_status("Ready. Connect a USB 3 USRP B200 to begin.")?;
        }
        WorkerToMain::ApplicationSpecific(event) => match event {
            SurveyEvent::Plot(plot) => {
                let sweep = plot.completed_sweeps;
                let result = render_summary(plot);
                LATEST_SWEEP.with(|s| s.set(sweep));
                enable_downloads(true);
                if EXPORT_PARTS.with(|p| p.borrow().is_some()) {
                    get_button(ID_DOWNLOAD_DATA)?.set_disabled(true);
                }
                send_message(MainToWorker::ApplicationSpecific(SurveyCommand::Rendered(
                    sweep,
                )))
                .await
                .map_err(display_error)?;
                result?;
            }
            SurveyEvent::Progress { text, retrying } => {
                set_status(&text)?;
                get_button(ID_STOP)?.set_text_content(Some(if retrying {
                    "Stop retrying"
                } else {
                    "Stop after sweep"
                }));
            }
            SurveyEvent::Finished => {
                RUNNING.with(|r| r.set(false));
                set_running_controls(false)?;
                if EXPORT_PARTS.with(|p| p.borrow().is_some()) {
                    get_button(ID_START)?.set_disabled(true);
                }
            }
            SurveyEvent::Failed(reason) => {
                warn!("Survey failed: {reason}");
                set_status(&format!("Survey failed: {reason}"))?;
                EXPORT_PARTS.with(|p| *p.borrow_mut() = None);
                get_button(ID_DOWNLOAD_DATA)?.set_disabled(LATEST_SWEEP.with(Cell::get) == 0);
            }
            SurveyEvent::ExportChunk(bytes) => EXPORT_PARTS.with(|slot| {
                if let Some(parts) = slot.borrow().as_ref() {
                    // Copy only this chunk to JS-owned memory. No complete text
                    // snapshot or second IQ/summary representation in WASM.
                    parts.push(&js_sys::Uint8Array::from(bytes.as_slice()));
                }
            }),
            SurveyEvent::ExportComplete => {
                let parts = EXPORT_PARTS.with(|p| p.borrow_mut().take());
                if let Some(parts) = parts {
                    let options = BlobPropertyBag::new();
                    options.set_type("text/plain;charset=utf-8");
                    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
                    let url = Url::create_object_url_with_blob(&blob)?;
                    let result = trigger_download(&url, "rf-survey-summary.txt");
                    Url::revoke_object_url(&url)?;
                    get_button(ID_DOWNLOAD_DATA)?.set_disabled(false);
                    get_button(ID_START)?.set_disabled(RUNNING.with(Cell::get));
                    result?;
                }
            }
        },
        WorkerToMain::LogLine { .. } => {}
        other => info!("Ignoring worker message: {other:?}"),
    }
    Ok(())
}

fn handle_start() -> Result<(), JsValue> {
    let requested = RequestedSettings::from_form()?.validate_before_usb()?;
    let widths = plot_widths()?;
    set_running_controls(true)?;
    RUNNING.with(|r| r.set(true));
    LATEST_SWEEP.with(|s| s.set(0));
    with_plot(&DB_PLOT, |p| p.clear())?;
    with_plot(&LINEAR_PLOT, |p| p.clear())?;
    enable_downloads(false);
    set_status("Opening the WebUSB device chooser…")?;
    spawn_local(async move {
        let result = async {
            let device = uhd_pure::b2xx::request_device()
                .await
                .map_err(display_error)?
                .ok_or_else(|| js_error("USRP chooser was cancelled"))?;
            let key = DeviceKey {
                vendor_id: device.vendor_id,
                product_id: device.product_id,
                serial: device.serial_number,
            };
            send_message(MainToWorker::Start((key, requested, widths)))
                .await
                .map_err(display_error)
        }
        .await;
        if let Err(error) = result {
            RUNNING.with(|r| r.set(false));
            let _ = set_running_controls(false);
            let detail = error.as_string().unwrap_or_else(|| format!("{error:?}"));
            let _ = set_status(&format!("Could not start survey: {detail}"));
        }
    });
    Ok(())
}

fn handle_stop() -> Result<(), JsValue> {
    command(SurveyCommand::Stop);
    get_button(ID_STOP)?.set_disabled(true);
    set_status("Stop requested.")
}

fn handle_download_data() -> Result<(), JsValue> {
    EXPORT_PARTS.with(|p| *p.borrow_mut() = Some(js_sys::Array::new()));
    get_button(ID_DOWNLOAD_DATA)?.set_disabled(true);
    get_button(ID_START)?.set_disabled(true);
    command(SurveyCommand::Export);
    Ok(())
}

fn render_summary(plot: PlotData) -> Result<(), JsValue> {
    let regions = wifi_regions();
    with_plot(&DB_PLOT, |p| p.update_envelope(plot.db, regions.clone()))?;
    with_plot(&LINEAR_PLOT, |p| p.update_envelope(plot.linear, regions))?;
    if !RUNNING.with(Cell::get) {
        set_status(&format!(
            "Rendered {} bins after {} complete sweeps.",
            plot.bins, plot.completed_sweeps
        ))?;
    }
    Ok(())
}
fn handle_theme_toggle() -> Result<(), JsValue> {
    let dark = !current_theme_is_dark()?;
    apply_theme_override(dark)?;
    with_plot(&DB_PLOT, |plot| plot.redraw())?;
    with_plot(&LINEAR_PLOT, |plot| plot.redraw())?;
    Ok(())
}

fn current_theme_is_dark() -> Result<bool, JsValue> {
    let document = web_sys::window()
        .ok_or_else(|| js_error("no browser window"))?
        .document()
        .ok_or_else(|| js_error("no browser document"))?;
    let root = document
        .document_element()
        .ok_or_else(|| js_error("document has no root element"))?;
    let classes = root.class_list();
    if classes.contains("rr-theme-dark") {
        return Ok(true);
    }
    if classes.contains("rr-theme-light") {
        return Ok(false);
    }
    Ok(web_sys::window()
        .ok_or_else(|| js_error("no browser window"))?
        .match_media("(prefers-color-scheme: dark)")?
        .is_some_and(|media| media.matches()))
}

fn apply_theme_override(dark: bool) -> Result<(), JsValue> {
    let document = web_sys::window()
        .ok_or_else(|| js_error("no browser window"))?
        .document()
        .ok_or_else(|| js_error("no browser document"))?;
    let root = document
        .document_element()
        .ok_or_else(|| js_error("document has no root element"))?;
    let classes = root.class_list();
    classes.remove_2("rr-theme-light", "rr-theme-dark")?;
    classes.add_1(if dark {
        "rr-theme-dark"
    } else {
        "rr-theme-light"
    })?;
    update_theme_button(dark)
}

fn update_theme_button(dark: bool) -> Result<(), JsValue> {
    let button = get_button(ID_THEME_TOGGLE)?;
    let (label, icon) = if dark {
        ("Switch to light mode", "☀")
    } else {
        ("Switch to dark mode", "☾")
    };
    button.set_attribute("aria-label", label)?;
    button.set_attribute("title", label)?;
    button.set_text_content(Some(icon));
    Ok(())
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

fn set_status(message: &str) -> Result<(), JsValue> {
    get_element(ID_STATUS)?.set_text_content(Some(message));
    Ok(())
}

fn set_running_controls(running: bool) -> Result<(), JsValue> {
    get_button(ID_START)?.set_disabled(running);
    let stop = get_button(ID_STOP)?;
    stop.set_disabled(!running);
    stop.set_text_content(Some("Stop after sweep"));
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
    update_theme_button(current_theme_is_dark()?)?;
    {
        let handler = Closure::<dyn FnMut(Event)>::new(|_| {
            if let Err(error) = handle_theme_toggle() {
                warn!("Theme change failed: {error:?}");
            }
        });
        get_button(ID_THEME_TOGGLE)?
            .add_event_listener_with_callback("click", handler.as_ref().unchecked_ref())?;
        handler.forget();
    }
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

    let resize = Closure::<dyn FnMut(Event)>::new(|_| {
        let generation = RESIZE_GENERATION.with(|g| {
            let next = g.get().wrapping_add(1);
            g.set(next);
            next
        });
        spawn_local(async move {
            Delay::new(Duration::from_millis(150)).await;
            if RESIZE_GENERATION.with(Cell::get) == generation
                && let Ok(widths) = plot_widths()
            {
                command(SurveyCommand::Viewport(widths));
            }
        });
    });
    web_sys::window()
        .ok_or_else(|| js_error("no window"))?
        .add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref())?;
    resize.forget();
    enable_downloads(false);
    let _worker =
        start_worker::<crate::MainApplication, crate::WorkerApplication, _, _>(worker_msg);
    Ok(())
}
