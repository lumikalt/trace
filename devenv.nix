{
  pkgs,
  lib,
  config,
  inputs,
  ...
}: {
  packages = [pkgs.circt pkgs.iverilog];

  languages.rust = {
    enable = true;
    channel = "nightly";
  };

  scripts = {
    # `trace` on PATH inside the shell, always reflecting the latest
    # source — no separate build step to remember, no stale binary if you
    # forget to rebuild after an edit. `-q` keeps cargo's own
    # "Compiling.../Finished" progress lines off stdout (real compile
    # errors still print), so `trace file.tr --firrtl` etc. stay pipeable.
    # This is what editors/vscode's formatter shells out to by default.
    trace.exec = ''
      exec cargo run -q -- "$@"
    '';

    # Run the test suite with the address space capped at 4 GiB, so a
    # runaway loop aborts fast instead of stalling the machine until the
    # OOM killer steps in (ulimit -v takes KiB).
    t.exec = ''
      ulimit -v 4194304
      exec cargo test "$@"
    '';
    # Run a .tr file all the way to a passing/failing simulation: lower ->
    # emit FIRRTL -> firtool -> Verilog -> iverilog/vvp against a
    # hand-written testbench at sim/<name>_tb.v. `-DSYNTHESIS` skips
    # firtool's debug-only register-randomization boilerplate, which uses
    # an `automatic`-lifetime construct Icarus doesn't support; skipping
    # it is safe since our registers are all properly reset regardless.
    # `-lowering-options=disallowLocalVariables` is the same Icarus-
    # compatibility idea applied to a DIFFERENT source of `automatic`
    # locals: a cross-width intermediate (e.g. one `div`/`rem` operand
    # zero-extended to match the other's width) can lower to an
    # `automatic logic` declared INSIDE an `always` block instead of a
    # top-level `wire`, hit by examples/div_rem.tr's differing-width case
    # with tests/sim.rs's own (non---disable-opt) firtool invocation — see
    # sim/README.md. Harmless here too even when `--disable-opt` alone
    # already avoids it (confirmed empirically): same design, just a
    # `wire` instead of an `automatic logic` either way.
    simulate.exec = ''
      set -euo pipefail
      if [ -z "''${1:-}" ]; then
        echo "usage: simulate <name>   (expects examples/<name>.tr and sim/<name>_tb.v)" >&2
        exit 1
      fi

      # firtool/iverilog CLI behavior has already drifted once for this
      # project silently (a firtool bump started needing `-format=fir` for
      # stdin, with no clear signal until something downstream broke in a
      # confusing way) -- see sim/README.md and TODO.md's "Simulation"
      # section. `packages` above pins the exact nixpkgs revision that
      # resolves to these tool versions, but that pin is only as good as
      # someone noticing a future `devenv update` moved it; this check
      # makes a version drift a clear, immediate failure right here
      # instead of a confusing one three tools downstream.
      want_firtool="firtool-1.147.0"
      got_firtool=$(firtool --version | grep -o 'firtool-[0-9.]*' || true)
      if [ "$got_firtool" != "$want_firtool" ]; then
        echo "simulate: expected $want_firtool, found '$got_firtool' -- firtool's CLI flags and output shape this project's scripts/tests assume may have changed; re-verify against sim/README.md before updating this pin" >&2
        exit 1
      fi
      want_iverilog="13.0"
      # Captured into a variable, not piped live through `head -1`: `-V`
      # keeps writing past its first line, and `head` closing the pipe
      # early sends the still-writing process SIGPIPE -- a real failure
      # under `pipefail`, even though the version line was already read.
      iverilog_version_output=$(iverilog -V 2>&1)
      got_iverilog=$(printf '%s\n' "$iverilog_version_output" | head -1 | sed -n 's/.*version \([0-9.]*\).*/\1/p')
      if [ "$got_iverilog" != "$want_iverilog" ]; then
        echo "simulate: expected iverilog $want_iverilog, found '$got_iverilog'" >&2
        exit 1
      fi

      name="$1"
      dir=$(mktemp -d)
      trap 'rm -rf "$dir"' EXIT
      cargo run -q -- "examples/$name.tr" --elaborate > "$dir/elaborated.tr"
      cargo run -q -- "$dir/elaborated.tr" --lower > "$dir/lowered.tr"
      cargo run -q -- "$dir/lowered.tr" --firrtl > "$dir/design.fir"
      firtool --disable-opt -lowering-options=disallowLocalVariables "$dir/design.fir" -o "$dir/design.v"

      # An `extmodule Name from "path.v"` declaration's `.v` implementation
      # is opaque data trace's own FIRRTL output never references at all
      # (see sim/README.md's "extmodule_tribuf_tb.v needs a second Verilog
      # source") -- iverilog needs it passed in directly, alongside the
      # generated design. Convention (this script's own, since trace's
      # compiler deliberately resolves no path itself): every such path is
      # resolved relative to sim/, matching where tribuf.v itself already
      # lives. Scanning the ORIGINAL example source, not the elaborated/
      # lowered intermediates -- an `extmodule` item is untouched by either
      # pass, but the original is the one guaranteed to exist regardless.
      extmodule_srcs=()
      while IFS= read -r extmodule_path; do
        v="sim/$extmodule_path"
        if [ ! -f "$v" ]; then
          echo "simulate: examples/$name.tr's \`extmodule ... from \"$extmodule_path\"\` has no matching sim/$extmodule_path" >&2
          exit 1
        fi
        extmodule_srcs+=("$v")
      done < <(grep -oE 'extmodule +[A-Za-z_][A-Za-z0-9_]* +from +"[^"]*"' "examples/$name.tr" | sed -E 's/.*from +"([^"]*)"/\1/' || true)

      iverilog -g2012 -DSYNTHESIS -o "$dir/sim" "sim/''${name}_tb.v" "$dir/design.v" "''${extmodule_srcs[@]:-}"
      vvp "$dir/sim"
    '';
  };

  enterTest = ''
    t
  '';

  git-hooks.hooks = {
    rustfmt.enable = true;
    cargo-check.enable = true;
    clippy.enable = true;
    alejandra.enable = true;
    statix.enable = true;
  };
}
