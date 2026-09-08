import { bootstrap } from "./rustradio-ui-bootstrap.js";

await (globalThis.coiReady ?? Promise.resolve());

await bootstrap({
  pkgName: "rf_survey_wasm",
  wasmMemoryConfig: {
    initial: 31,
    maximum: 16384,
    shared: true,
  },
  workerThreadStackSize: 1024 * 1024,
});
