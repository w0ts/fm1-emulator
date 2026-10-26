#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
# A card image for the emulator's SD card (src/sd.rs; fm1-ui --sd IMAGE): an MBR and one FAT32 volume, as a
# computer formats a card, filled from a zip unpacked at its root (Optimist's optimist-sd-card.zip: the OPTIMIST
# folders and the factory presets) and WAVs copied to OPTIMIST/SAMPLES. macOS (hdiutil / diskutil).
#   sd_card_image.sh IMAGE [SIZE_MB] [ZIP] [WAV...]
set -eu
img=$1
mb=${2:-128}
zip=${3:-}
[ $# -ge 3 ] && shift 3 || shift $#
[ -e "$img" ] && { echo "sd_card_image: $img exists" >&2; exit 2; }
mkfile -n "${mb}m" "$img"
dev=$(diskutil image attach --noMount "$img" | grep -o '/dev/disk[0-9]*' | head -1)
diskutil eraseDisk FAT32 OPTIMIST MBRFormat "$dev" >/dev/null
vol=$(diskutil info "${dev}s1" | sed -n 's/^ *Mount Point: *//p')
mdutil -i off "$vol" >/dev/null 2>&1 || true
[ -n "$zip" ] && unzip -q -o "$zip" -d "$vol"
mkdir -p "$vol/OPTIMIST/SAMPLES"
for w in "$@"; do cp -X "$w" "$vol/OPTIMIST/SAMPLES/"; done
find "$vol/OPTIMIST" -name '._*' -delete
diskutil eject "$dev" >/dev/null
echo "sd_card_image: $img (${mb} MB, FAT32)"
