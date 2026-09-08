# rf-survey-wasm

Browser-based broadband RF survey tool for a USB 3 USRP B200 revision 5 or
newer. It uses `uhd-pure` through WebUSB, performs Blackman-Harris PSD averaging
in a WASM worker, and redraws cumulative average/maximum dB and linear-power
plots after every complete sweep.

The tool currently uses local development patches from sibling checkouts of
`../uhd-pure` and `../rustradio`. Dependency sources are not vendored here.

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

If the B200 is initially in its FX3 bootloader, the first click loads firmware.
After it re-enumerates, click **Connect & start** again and choose the B200's
firmware-backed identity.

## Output

The live graphs show cumulative per-frequency average and maximum PSD. The LO
offset alternates between sweeps by default so that, once both paths have data,
receiver images can be rejected in the same way as native `rf-survey`.

After the first complete sweep, the page can download both plot canvases as PNG
and a native-compatible summary text file:

```text
# frequency_hz average_power_dbfs_per_hz maximum_power_dbfs_per_hz observations
```
