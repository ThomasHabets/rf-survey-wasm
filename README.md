# rf-survey-wasm

Browser-based broadband RF survey tool for a USB 3 USRP B200 revision 5 or
newer. It uses `uhd-pure` through WebUSB, performs Blackman-Harris PSD averaging
in a WASM worker, and redraws cumulative average/maximum dB and linear-power
plots after every complete sweep.

Live demo at <https://rf-survey.habets.se/>.

## Build

Install `wasm-pack`, `jq`, and the UHD images package, then run:

```console
./build-local.sh release
tar xzf rf-survey-wasm.tgz
./rf-survey-wasm/serve.py rf-survey-wasm
```

Set `UHD_IMAGES_DIR` if the B200 firmware and FPGA images are not installed in
`/usr/share/uhd/images`. The images are copied only into the generated archive;
they are not checked into this repository.

Open `http://localhost:8000` in a Chromium-family browser. WebUSB requires a
secure context (localhost or HTTPS), and the shared-memory worker requires the
cross-origin isolation headers supplied by `serve.py`.

## GitHub Pages

`build-pages.sh` creates a deployable `dist/` directory. The Pages workflow
builds and deploys it after pushes to `main`, or when started manually. The
deployed site uses a same-origin service worker to add the COOP/COEP headers
that GitHub Pages cannot configure directly.

The workflow downloads only the official B200 firmware and FPGA image during
the build; those files and the patched dependency sources are not vendored in
this repository.

The current performance work uses sibling checkouts of `rustradio` 0.18.3
(`rustradio-ui` 0.1.24) and `uhd-pure` 0.1.5 through `Cargo.toml` patches.
Publish those versions before removing the patches; until then, the Pages
workflow checks out both sibling repositories beside this one. The dependency
commits therefore need to be on those repositories' default branches before
this repository's Pages workflow can build them.

If the B200 is initially in its FX3 bootloader, the first click loads firmware.
After it re-enumerates, click **Connect & start** again and choose the B200's
firmware-backed identity.

## Output

The live graphs show cumulative per-frequency average and maximum PSD. The LO
offset alternates between sweeps by default so that, once both paths have data,
receiver images can be rejected in the same way as native `rf-survey`.

Use the sun/moon button in the top-right corner to switch between light and dark
mode. The page and both canvas graphs redraw immediately, including while a
survey is running.

WebUSB capture and FFT processing both run off the browser's main thread. USB
reads are queued and their buffers are reused; settling samples are discarded
before decoding, and FFT working storage is reused. Reception stops before DSP
starts so CPU-heavy transforms cannot starve active WebUSB reads. A
discontinuous dwell is discarded and retried indefinitely. Every ten
consecutive failures add a 1, 2, 4, 8, then capped 10-second cooldown before
retrying the same band, without flooding the log.

Only a display-width min/max envelope is sent to the main thread after each
complete sweep. The full-resolution cumulative result remains in the worker
and is streamed out in bounded chunks when summary data is downloaded.

After the first complete sweep, the page can download both plot canvases as PNG
and a native-compatible summary text file:

```text
# frequency_hz average_power_dbfs_per_hz maximum_power_dbfs_per_hz observations
```
