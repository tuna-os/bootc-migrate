#!/usr/bin/env bash
# Capture TUI screenshots for the tuna-os/docs website.
#
# NOTE: the E2E tui-migrate cell now produces walkthrough artifacts
# automatically on every run — asciicast recordings, timelapse GIFs, and
# per-screen text screenshots (see docs/testing.md "TUI testing", the
# tui-walkthrough CI artifact, and tests/tui-e2e-driver.py --record /
# --snapshot-dir). This script remains for styled PNG captures via vhs.
#
# Prerequisites:
#   - Install `vhs` (https://github.com/charmbracelet/vhs)
#     brew install vhs  OR  go install github.com/charmbracelet/vhs@latest
#   - The binary must be built and deployed on the tui-e2e corral VM
#   - Or run locally on an OSTree system (will show real preflight data)
#
# Usage:
#   ./scripts/capture-screenshots.sh [output_dir]
#
# Output:
#   PNG files matching the docs site expectations:
#     dakota-migrate-welcome.png
#     dakota-migrate-preflight.png  (NEW)
#     dakota-migrate-select-image.png
#     dakota-migrate-options.png
#     dakota-migrate-review.png
#     dakota-migrate-running.png
#     dakota-migrate-complete.png

set -euo pipefail

OUTPUT_DIR="${1:-/home/james/dev/tuna-os/docs/static/img/screenshots}"
mkdir -p "$OUTPUT_DIR"

if ! command -v vhs &>/dev/null; then
    echo "ERROR: 'vhs' not installed. Install from https://github.com/charmbracelet/vhs"
    echo ""
    echo "Alternative: use the Python PTY capture on a corral VM:"
    echo "  corral ssh tui-e2e --user root -c 'python3 /tmp/capture_tui.py'"
    echo ""
    echo "For proper PNG screenshots, install vhs and re-run this script."
    exit 1
fi

# Generate VHS tape file for each screen
cat > /tmp/bmc-screenshots.tape << 'TAPE'
# bootc-migrate TUI screenshot capture
Output /tmp/bmc-frames/welcome.png
Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 800
Set Theme "Dracula"

Type "sudo bootc-migrate"
Enter
Sleep 2s
Screenshot /tmp/bmc-frames/dakota-migrate-welcome.png

# Navigate to Preflight
Type "\r"
Sleep 5s
Screenshot /tmp/bmc-frames/dakota-migrate-preflight.png

# Navigate to Select Image
Type "\r"
Sleep 1s
Screenshot /tmp/bmc-frames/dakota-migrate-select-image.png

# Navigate to Options
Type "\r"
Sleep 1s
Screenshot /tmp/bmc-frames/dakota-migrate-options.png

# Navigate to Review
Type "n"
Sleep 1s
Screenshot /tmp/bmc-frames/dakota-migrate-review.png

# Quit
Type "q"
Sleep 500ms
Type "h"
Type "\r"
TAPE

echo "Running VHS tape..."
mkdir -p /tmp/bmc-frames
vhs /tmp/bmc-screenshots.tape

echo "Copying screenshots to $OUTPUT_DIR..."
cp /tmp/bmc-frames/dakota-migrate-*.png "$OUTPUT_DIR/"

echo "Done! Screenshots saved to:"
ls -la "$OUTPUT_DIR"/dakota-migrate-*.png
