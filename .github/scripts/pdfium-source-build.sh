#!/usr/bin/env bash
# Isolated upstream compile; never installs a production native artifact.
set -euo pipefail
project=$(pwd)
build_root=${PDFIUM_SOURCE_BUILD_ROOT:?set an absolute, empty build directory}
evidence=${TPE_PDFIUM_PROBE_EVIDENCE_DIR:?set the evidence directory}
[[ "$build_root" = /* && "$evidence" = /* ]]
mkdir -p "$build_root" "$evidence"
[[ -z $(ls -A "$build_root") ]]
pin="$project/native/pdfium-source.json"
revision=$(jq -er '.revision | select(test("^[0-9a-f]{40}$"))' "$pin")
repository=$(jq -er '.repository | select(. == "https://pdfium.googlesource.com/pdfium.git")' "$pin")
depot_repository=$(jq -er '.depot_tools_repository | select(. == "https://chromium.googlesource.com/chromium/tools/depot_tools.git")' "$pin")
cp "$pin" "$evidence/source-pin.json"
git rev-parse HEAD > "$evidence/code-sha.txt"
date -u +%FT%TZ > "$evidence/started-at.txt"
df -h > "$evidence/disk-before.txt"
uname -a > "$evidence/host.txt"
cat /etc/os-release >> "$evidence/host.txt"
printf 'ImageOS=%s\nImageVersion=%s\n' "${ImageOS:-unknown}" "${ImageVersion:-unknown}" >> "$evidence/host.txt"

checkout_revision() {
  local url=$1 revision=$2 directory=$3
  git init "$directory"
  git -C "$directory" remote add origin "$url"
  git -C "$directory" fetch --depth=1 origin "$revision"
  git -C "$directory" checkout --detach FETCH_HEAD
  [[ $(git -C "$directory" rev-parse HEAD) = "$revision" ]]
}
checkout_revision "$repository" "$revision" "$build_root/pdfium"
cp "$build_root/pdfium/DEPS" "$evidence/DEPS"
# Read only literal vars with AST; do not execute a fetched DEPS file to discover
# the bootstrap tool. Its full SHA is part of the reviewed source commit.
depot_revision=$(python3 - "$build_root/pdfium/DEPS" <<'PY'
import ast
import pathlib
import re
import sys

tree = ast.parse(pathlib.Path(sys.argv[1]).read_text())
assignments = [node for node in tree.body if isinstance(node, ast.Assign)
               and any(isinstance(name, ast.Name) and name.id == "vars"
                       for name in node.targets)]
if len(assignments) != 1:
    raise SystemExit("expected one vars assignment in pinned DEPS")
variables = assignments[0].value
if not isinstance(variables, ast.Dict):
    raise SystemExit("expected literal vars dictionary in pinned DEPS")
pins = [value for key, value in zip(variables.keys, variables.values)
        if isinstance(key, ast.Constant) and key.value == "depot_tools_revision"]
if len(pins) != 1:
    raise SystemExit("expected one depot_tools_revision in pinned DEPS")
revision = ast.literal_eval(pins[0])
if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
    raise SystemExit("DEPS must pin a full depot_tools SHA")
print(revision)
PY
)
printf '%s\n' "$depot_revision" > "$evidence/depot-tools-sha.txt"
checkout_revision "$depot_repository" "$depot_revision" "$build_root/depot_tools"
export PATH="$build_root/depot_tools:$PATH"
export DEPOT_TOOLS_UPDATE=0 DEPOT_TOOLS_WIN_TOOLCHAIN=0
cd "$build_root"
gclient config --unmanaged "$repository" --custom-var checkout_configuration=minimal
gclient sync --revision "pdfium@$revision" --no-history --shallow
[[ $(git -C pdfium rev-parse HEAD) = "$revision" ]]
[[ $(git -C depot_tools rev-parse HEAD) = "$depot_revision" ]]
[[ $(git -C pdfium/third_party/depot_tools rev-parse HEAD) = "$depot_revision" ]]
# Chromium GN/python wrappers discover this DEPS checkout even when bootstrap
# gclient came from the separate tools directory. Initialize its pinned Python
# and CIPD packages too, without enabling tool self-updates.
(cd pdfium/third_party/depot_tools && ./ensure_bootstrap)
export PATH="$build_root/pdfium/third_party/depot_tools:$PATH"
gclient revinfo -a > "$evidence/dependency-revisions.txt"
cp .gclient "$evidence/gclient.txt"
cd pdfium
build/install-build-deps.sh --no-prompt
dpkg-query -W > "$evidence/host-packages.txt"
gclient runhooks
build/linux/sysroot_scripts/install-sysroot.py --arch=x64
# A single shared library preserves the prebuilt library's deployment shape.
# Only public-symbol export and the target type change; no parser patch.
git apply --check "$project/native/pdfium-source-shared.patch"
git apply "$project/native/pdfium-source-shared.patch"
git diff > "$evidence/source-changes.patch"
mkdir -p out/Release
cat > out/Release/args.gn <<'GN'
is_debug = false
is_component_build = false
pdf_is_standalone = true
pdf_use_partition_alloc = false
pdf_enable_v8 = false
pdf_enable_xfa = false
clang_use_chrome_plugins = false
target_os = "linux"
target_cpu = "x64"
use_remoteexec = false
use_siso = false
GN
cp out/Release/args.gn "$evidence/args.gn"
gn gen out/Release
gn --version > "$evidence/gn-version.txt"
ninja --version > "$evidence/ninja-version.txt"
third_party/llvm-build/Release+Asserts/bin/clang --version > "$evidence/clang-version.txt"
sha256sum third_party/llvm-build/Release+Asserts/bin/clang > "$evidence/clang-sha256.txt"
if [[ -f third_party/llvm-build/Release+Asserts/cr_build_revision ]]; then
  cp third_party/llvm-build/Release+Asserts/cr_build_revision "$evidence/clang-build-revision.txt"
fi
ninja -C out/Release -j 2 pdfium
[[ -f out/Release/libpdfium.so ]]
sha256sum out/Release/libpdfium.so > "$evidence/library-sha256.txt"
file out/Release/libpdfium.so > "$evidence/library-file.txt"
ldd out/Release/libpdfium.so > "$evidence/library-dependencies.txt"
du -sh "$build_root" > "$evidence/build-disk.txt"
df -h > "$evidence/disk-after.txt"
date -u +%FT%TZ > "$evidence/built-at.txt"
