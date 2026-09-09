use log::info;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

mod acquisition;
mod mainthread;
mod model;
mod settings;
mod worker;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MainApplication;

impl rustradio_ui::ApplicationSpecific for MainApplication {
    type App = SurveyCommand;
    type Start = (model::DeviceKey, settings::RequestedSettings, [usize; 2]);
    type End = rustradio_ui::AppEmpty;
    type Ready = rustradio_ui::AppEmpty;
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WorkerApplication;

impl rustradio_ui::ApplicationSpecific for WorkerApplication {
    type App = SurveyEvent;
    type Start = rustradio_ui::AppEmpty;
    type End = rustradio_ui::AppEmpty;
    type Ready = rustradio_ui::AppEmpty;
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SurveyCommand {
    Stop,
    Viewport([usize; 2]),
    Export,
    Rendered(u64),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SurveyEvent {
    Plot(model::PlotData),
    Progress { text: String, retrying: bool },
    Finished,
    ExportChunk(Vec<u8>),
    ExportComplete,
    Failed(String),
}

pub(crate) type MainToWorker = rustradio_ui::MainToWorker<MainApplication>;
pub(crate) type WorkerToMain = rustradio_ui::WorkerToMain<WorkerApplication>;

#[wasm_bindgen]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    if web_sys::window().is_none() {
        info!("Worker: starting");
        worker::setup().await
    } else {
        rustradio_ui::dom_logger::init_logging::<WorkerApplication>(
            mainthread::ID_LOG_OUTPUT,
            log::LevelFilter::Info,
        )
        .map_err(|error| JsValue::from_str(&format!("{error:?}")))?;
        info!("Main UI: starting");
        mainthread::setup().await
    }
}
