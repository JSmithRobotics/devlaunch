#!/usr/bin/env bash
#
# Runs scripts/public-api-snapshots.sh with everything it needs, so that "run
# the public-API gate" is one command a developer can actually type -- `pixi
# run public-api` -- and CI can run the identical thing rather than reaching
# for a nightly toolchain of its own. What this script adds on top of the
# regeneration script itself:
#
#   1. Installs the pinned `cargo-public-api`, into a directory under
#      rust/target so it rides the same cache CI already keeps warm for the
#      Rust build (rust/target/ is gitignored and Swatinem/rust-cache keys on
#      it), and skips the install outright when that exact pin is already
#      there -- a task that reinstalls a Rust tool on every invocation is not
#      one anyone runs by hand.
#   2. Puts scripts/public-api-rustup-shim on PATH ahead of everything else,
#      and sets RUSTC_BOOTSTRAP=1. Together those are what let this
#      repository's pinned *stable* toolchain stand in for the nightly
#      `cargo public-api` otherwise insists on; see the comment at the top of
#      scripts/public-api-rustup-shim/rustup for exactly what each piece is
#      doing and why neither one alone is enough.
#
# Any argument this script is given is forwarded to public-api-snapshots.sh
# untouched -- a DEST directory, or one of its `--print-*`/`--classify` modes
# -- so `scripts/run-public-api.sh` and `pixi run public-api` behave exactly
# like `scripts/public-api-snapshots.sh` itself, just with the environment it
# needs already assembled.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
snapshots_script="$repo_root/scripts/public-api-snapshots.sh"
shim_dir="$repo_root/scripts/public-api-rustup-shim"

pin="$("$snapshots_script" --print-pin)"

# Installed here rather than into the ambient cargo home: a version pinned for
# rendering these three snapshots has nothing to do with whatever
# `cargo-public-api` a developer might otherwise have on their own machine,
# and installing over that would be a surprising thing for a repo task to do.
tool_root="$repo_root/rust/target/public-api-tools"
tool_bin="$tool_root/bin"

installed=""
if [[ -x "$tool_bin/cargo-public-api" ]]; then
  installed="$("$tool_bin/cargo-public-api" --version 2>/dev/null || true)"
fi
if [[ "$installed" != "cargo-public-api $pin" ]]; then
  echo "public-api: installing cargo-public-api $pin (found: '${installed:-nothing}')" >&2
  # The link failure this works around, and why it is set *here* rather than
  # on the outside of a `pixi run` invocation: this repository's conda-provided
  # Rust toolchain links with `x86_64-conda-linux-gnu-cc` by default, and that
  # linker fails on this tool with "undefined symbol: __libc_csu_init" against
  # the conda sysroot's `Scrt1.o`. `/usr/bin/gcc` links it fine. Setting
  # `CC`/`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER` on the command line of a
  # `pixi run` invocation that wraps this script does NOT work: pixi's own
  # activation script sets `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=
  # x86_64-conda-linux-gnu-cc` as part of entering the environment, and that
  # runs *after* an outer environment variable would have been inherited, so it
  # silently wins and the outer override is never seen by cargo. Setting it
  # here, inside the process pixi already started and activated, is what
  # actually reaches `cargo install` below. (Only the install needs this: once
  # built, running `cargo-public-api` itself -- the `cargo rustdoc` it shells
  # out to -- links nothing and needs no override, which is why the exported
  # environment further down does not carry it.)
  CC=/usr/bin/gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc \
    cargo install cargo-public-api --locked --version "$pin" --root "$tool_root"
fi

export PATH="$shim_dir:$tool_bin:$PATH"
# Lets the pinned *stable* rustc accept the unstable rustdoc-JSON flags
# `cargo public-api` passes it, standing in for the nightly toolchain it would
# otherwise require. See the header of scripts/public-api-snapshots.sh for why
# this is the deliberate, permanent way these snapshots are rendered now, and
# why regenerating them under an actual nightly instead would produce a
# spurious whole-file diff rather than a real one.
export RUSTC_BOOTSTRAP=1

exec "$snapshots_script" "$@"
