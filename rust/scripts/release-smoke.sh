#!/usr/bin/env bash
# The packaged release smoke path for the Rust desktop shell.
#
# The cutover's acceptance is that a *packaged* build starts the Rust daemon,
# serves the web export, and completes the packaged smoke path. This script is
# that path: it stages the export, builds the bundle, runs the binary out of
# the bundle, and asks the running app for the web app.
#
# What each check proves:
#
#   * the export reached the bundle — the staged export carries a run marker,
#     and the page the daemon serves must contain it. A page served from the
#     source checkout (the `apps/web/out` fallback) cannot carry the marker,
#     so a bundle that forgot to ship the export fails here instead of
#     silently serving the checkout.
#   * the daemon came up inside the packaged app — `GET /api/health` must
#     answer on the loopback port the app was given.
#   * the daemon serves the export — `GET /` must answer the export's shell.
#   * the app wrote into the namespaced data root — the namespace directory
#     must exist under the data root the run was given, which is the packaged
#     layout (`<base>/namespaces/<namespace>/data`).
#
# Usage:
#   rust/scripts/release-smoke.sh [--bundle <format>] [--export <dir>]
#                                 [--port <n>] [--skip-build] [--keep]
#
# The bundle format defaults to the platform's own (deb on Linux, app on
# macOS, nsis on Windows). `--export` stages a prepared export instead of the
# checkout's `apps/web/out`; without a real export the script stages the
# `host-probe` fixture and says so in the report.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rust_root="$(cd "$script_dir/.." && pwd)"
repo_root="$(cd "$rust_root/.." && pwd)"
app_dir="$rust_root/apps/od-tauri"
src_dir="$app_dir/src-tauri"
staged_export="$app_dir/web-export"
report_root="$repo_root/.tmp/release-report"

bundle_format=""
export_source=""
port=""
skip_build=0
keep=0

usage() {
  sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() {
  echo "release-smoke: $*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bundle)
      bundle_format="${2:-}"
      shift 2
      ;;
    --export)
      export_source="${2:-}"
      shift 2
      ;;
    --port)
      port="${2:-}"
      shift 2
      ;;
    --skip-build)
      skip_build=1
      shift
      ;;
    --keep)
      keep=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown option: $1"
      ;;
  esac
done

case "$(uname -s)" in
  Linux) platform=linux ;;
  Darwin) platform=macos ;;
  MINGW* | MSYS* | CYGWIN*) platform=windows ;;
  *) die "unsupported platform: $(uname -s)" ;;
esac

if [[ -z "$bundle_format" ]]; then
  case "$platform" in
    linux) bundle_format=deb ;;
    macos) bundle_format=app ;;
    windows) bundle_format=nsis ;;
  esac
fi

product_name="$(jq -r '.productName' "$src_dir/tauri.conf.json")"
app_version="$(jq -r '.version' "$src_dir/tauri.conf.json")"

# A free loopback port for the daemon. The shell reads `OD_PORT` and binds
# there, so the probe knows where to look.
free_port() {
  python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

if [[ -z "$port" ]]; then
  port="$(free_port)"
fi

run_root="$(mktemp -d "${TMPDIR:-/tmp}/od-release-smoke-XXXXXX")"
data_root="$run_root/data"
mkdir -p "$data_root"

cleanup() {
  if [[ -n "${app_pid:-}" ]] && kill -0 "$app_pid" 2>/dev/null; then
    kill "$app_pid" 2>/dev/null || true
    wait "$app_pid" 2>/dev/null || true
  fi
  if [[ "$keep" -eq 0 && -d "$run_root" ]]; then
    rm -rf "$run_root"
  else
    echo "release-smoke: kept $run_root" >&2
  fi
}
trap cleanup EXIT

# --- stage the export -------------------------------------------------------

marker="od-release-smoke-$(date +%s)-$$"
export_kind=fixture
rm -rf "$staged_export"
mkdir -p "$staged_export"
if [[ -n "$export_source" ]]; then
  [[ -f "$export_source/index.html" ]] || die "no index.html in $export_source"
  cp -R "$export_source/." "$staged_export/"
  export_kind=prepared
elif [[ -f "$repo_root/apps/web/out/index.html" ]]; then
  cp -R "$repo_root/apps/web/out/." "$staged_export/"
  export_kind=checkout
else
  cp "$app_dir/host-probe/index.html" "$staged_export/index.html"
fi
# The marker is what proves the served page came out of the bundle.
printf '\n<!-- %s -->\n' "$marker" >>"$staged_export/index.html"

# --- build the bundle -------------------------------------------------------

if [[ "$skip_build" -eq 0 ]]; then
  if [[ ! -x "$app_dir/node_modules/.bin/tauri" ]]; then
    echo "release-smoke: installing the tauri CLI" >&2
    (cd "$app_dir" && npm install --no-audit --no-fund)
  fi
  echo "release-smoke: building the $bundle_format bundle" >&2
  (cd "$app_dir" && ./node_modules/.bin/tauri build --bundles "$bundle_format" \
    --config '{"build":{"beforeBuildCommand":"true"}}')
fi

bundle_dir="$rust_root/target/release/bundle/$bundle_format"
[[ -d "$bundle_dir" ]] || die "no bundle in $bundle_dir"

binary=""
export_dir=""
case "$bundle_format" in
  deb)
    artifact="$(ls -1 "$bundle_dir"/*.deb 2>/dev/null | head -1)"
    [[ -n "$artifact" ]] || die "no .deb in $bundle_dir"
    root="$run_root/deb"
    mkdir -p "$root"
    dpkg-deb -x "$artifact" "$root"
    binary="$root/usr/bin/$product_name"
    export_dir="$root/usr/lib/$product_name/web"
    ;;
  rpm)
    artifact="$(ls -1 "$bundle_dir"/*.rpm 2>/dev/null | head -1)"
    [[ -n "$artifact" ]] || die "no .rpm in $bundle_dir"
    command -v rpm2cpio >/dev/null || die "rpm2cpio is needed to unpack an rpm"
    root="$run_root/rpm"
    mkdir -p "$root"
    (cd "$root" && rpm2cpio "$artifact" | cpio -idm)
    binary="$root/usr/bin/$product_name"
    export_dir="$root/usr/lib/$product_name/web"
    ;;
  app)
    artifact="$(ls -1d "$bundle_dir"/*.app 2>/dev/null | head -1)"
    [[ -n "$artifact" ]] || die "no .app in $bundle_dir"
    binary="$artifact/Contents/MacOS/$product_name"
    export_dir="$artifact/Contents/Resources/web"
    ;;
  nsis | msi)
    # The installer is not unpacked here: on Windows the bundler copies the
    # resources next to the binary it produces, so the binary *is* the
    # packaged layout.
    binary="$rust_root/target/release/$product_name.exe"
    export_dir="$rust_root/target/release/web"
    ;;
  appimage)
    artifact="$(ls -1 "$bundle_dir"/*.AppImage 2>/dev/null | head -1)"
    [[ -n "$artifact" ]] || die "no .AppImage in $bundle_dir"
    root="$run_root/appimage"
    mkdir -p "$root"
    (cd "$root" && "$artifact" --appimage-extract >/dev/null)
    binary="$root/squashfs-root/usr/bin/$product_name"
    export_dir="$root/squashfs-root/usr/lib/$product_name/web"
    ;;
  *)
    die "unsupported bundle format: $bundle_format"
    ;;
esac

[[ -x "$binary" ]] || die "the packaged binary is missing: $binary"
[[ -f "$export_dir/index.html" ]] || die "the bundle ships no web export: $export_dir"

# --- run the packaged app ---------------------------------------------------

display=""
if [[ "$platform" == linux ]]; then
  # The window needs a display; the smoke itself only talks to the daemon,
  # but the app refuses to start without one.
  if [[ -z "${DISPLAY:-}" ]]; then
    if ! command -v Xvfb >/dev/null; then
      die "Xvfb is needed to run the packaged app headlessly"
    fi
    Xvfb :99 -screen 0 1440x900x24 >"$run_root/xvfb.log" 2>&1 &
    xvfb_pid=$!
    display=":99"
    trap 'kill "$xvfb_pid" 2>/dev/null || true; cleanup' EXIT
  else
    display="$DISPLAY"
  fi
fi

echo "release-smoke: running $binary on port $port" >&2
env \
  OD_PORT="$port" \
  OD_DATA_DIR="$data_root" \
  DISPLAY="$display" \
  WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  LIBGL_ALWAYS_SOFTWARE=1 \
  RUST_LOG="${RUST_LOG:-info}" \
  "$binary" >"$run_root/app.log" 2>&1 &
app_pid=$!

health=""
for _ in $(seq 1 120); do
  if health="$(curl -fsS --max-time 2 "http://127.0.0.1:$port/api/health" 2>/dev/null)"; then
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    cat "$run_root/app.log" >&2 || true
    die "the packaged app exited before the daemon answered"
  fi
  sleep 0.5
done
[[ -n "$health" ]] || {
  cat "$run_root/app.log" >&2 || true
  die "the daemon did not answer /api/health"
}

curl -fsS --max-time 5 "http://127.0.0.1:$port/" -o"$run_root/index.html" \
  || die "the daemon did not serve the web export at /"
grep -q "$marker" "$run_root/index.html" \
  || die "the served page did not come from the bundle (marker $marker missing)"

namespace_dir=""
for candidate in "$data_root"/namespaces/*/; do
  [[ -d "$candidate" ]] || continue
  namespace_dir="${candidate%/}"
done
[[ -n "$namespace_dir" ]] || die "the app did not create a namespaced data root"
[[ -d "$namespace_dir/data" ]] || die "the namespaced data root has no data directory"

namespace="$(basename "$namespace_dir")"

# --- report -----------------------------------------------------------------

report_dir="$report_root/$platform"
mkdir -p "$report_dir"
jq -n \
  --arg platform "$platform" \
  --arg bundle "$bundle_format" \
  --arg version "$app_version" \
  --arg product "$product_name" \
  --arg export "$export_kind" \
  --arg namespace "$namespace" \
  --arg dataRoot "$namespace_dir/data" \
  --arg port "$port" \
  --arg health "$health" \
  --arg servedFrom "$export_dir" \
  '{platform: $platform, bundle: $bundle, version: $version, product: $product,
    export: $export, namespace: $namespace, dataRoot: $dataRoot, port: $port,
    health: $health, servedFrom: $servedFrom, ok: true}' \
  >"$report_dir/report.json"

echo "release-smoke: ok — the packaged app served the export out of $export_dir" >&2
echo "release-smoke: report at $report_dir/report.json" >&2
