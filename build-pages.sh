#!/usr/bin/env bash
set -euo pipefail

web_dir="web"
output_dir="${1:-dist}"
ui_manifest="$(cargo metadata --format-version=1 | jq -r '.packages[] | select(.name=="rustradio-ui") | .manifest_path' | head -n1)"
ui_dir="$(dirname "$ui_manifest")"
ui_assets="$ui_dir/assets"
uhd_images_dir="${UHD_IMAGES_DIR:-/usr/share/uhd/images}"
firmware="$uhd_images_dir/usrp_b200_fw.hex"
fpga="$uhd_images_dir/usrp_b200_fpga.bin"

if [[ -z "$output_dir" || "$output_dir" == "/" || "$output_dir" == "." ]]; then
  echo "Refusing unsafe Pages output directory: $output_dir" >&2
  exit 1
fi

for image in "$firmware" "$fpga"; do
  if [[ ! -r "$image" ]]; then
    echo "Missing required USRP B200 image: $image" >&2
    echo "Install/download the UHD images or set UHD_IMAGES_DIR." >&2
    exit 1
  fi
done

temporary_dir="$(mktemp -d)"
trap 'rm -rf -- "$temporary_dir"' EXIT
site_dir="$temporary_dir/site"

wasm-pack build --target web --release -d "$site_dir"
cp \
  "$web_dir/index.html" \
  "$web_dir/wasm-mod.js" \
  "$web_dir/coi-serviceworker.js" \
  "$web_dir/.nojekyll" \
  "$site_dir/"
cp "$firmware" "$fpga" "$site_dir/"
cp "$ui_assets/bootstrap.js" "$site_dir/rustradio-ui-bootstrap.js"
cat "$ui_assets/rustradio.css" "$web_dir/style.css" > "$site_dir/style.css"

rm -rf -- "$output_dir"
mkdir -p "$(dirname "$output_dir")"
mv "$site_dir" "$output_dir"
