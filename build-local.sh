#!/usr/bin/env bash
set -euo pipefail

web_dir="web"
prefix="rf-survey-wasm"
git_version="$(git rev-parse --short HEAD)"
ui_manifest="$(cargo metadata --format-version=1 | jq -r '.packages[] | select(.name=="rustradio-ui") | .manifest_path' | head -n1)"
ui_dir="$(dirname "$ui_manifest")"
ui_assets="$ui_dir/assets"
uhd_images_dir="${UHD_IMAGES_DIR:-/usr/share/uhd/images}"
firmware="$uhd_images_dir/usrp_b200_fw.hex"
fpga="$uhd_images_dir/usrp_b200_fpga.bin"

for image in "$firmware" "$fpga"; do
  if [[ ! -r "$image" ]]; then
    echo "Missing required USRP B200 image: $image" >&2
    echo "Install the UHD images package or set UHD_IMAGES_DIR." >&2
    exit 1
  fi
done

temporary_dir="$(mktemp -d)"
trap 'rm -rf -- "$temporary_dir"' EXIT
profile="${1:-profiling}"
wasm-pack build --target web -d "$temporary_dir/$prefix" "--$profile"
cp \
  "$web_dir/wasm-mod.js" \
  "$web_dir/coi-serviceworker.js" \
  "$web_dir/favicon.ico" \
  "$temporary_dir/$prefix/"
sed "s/@GIT_VERSION@/$git_version/g" "$web_dir/index.html" > "$temporary_dir/$prefix/index.html"
cp "serve.py" "$temporary_dir/$prefix/"
cp "$firmware" "$fpga" "$temporary_dir/$prefix/"
cp "$ui_assets/bootstrap.js" "$temporary_dir/$prefix/rustradio-ui-bootstrap.js"
cat "$ui_assets/rustradio.css" "$web_dir/style.css" > "$temporary_dir/$prefix/style.css"
(
  cd "$temporary_dir"
  tar czf - "$prefix"
) > "$prefix.tgz"
