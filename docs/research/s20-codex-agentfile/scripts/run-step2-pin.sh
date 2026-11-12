#!/usr/bin/env bash
# Plan step 20 [kiln], step 2's pin (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): step 1's run
# (run-step1.sh) again for the pinned tag, now that pin.json names the version smoke; then the ELF
# survey of both packages (elf-survey.sh) into OUT/elf.txt, its exit code into OUT/elf.rc.
#
#   run-step2-pin.sh OUT TAG       (run-step1.sh, its scripts and elf-survey.sh beside it)
set -uo pipefail

out=${1:?usage: run-step2-pin.sh OUT TAG}
tag=${2:?usage: run-step2-pin.sh OUT TAG}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
"$here/run-step1.sh" "$out" "$tag"
"$here/elf-survey.sh" "$out/pin.json" >"$out/elf.txt" 2>&1
echo $? >"$out/elf.rc"
