{
  pkgs,
  lib,
  config,
  inputs,
  ...
}: {
  # `python3`: not itself a simulator, but `verilator --binary`'s own build
  # step (`verilator_includer`) shells out to it, and it's not on PATH by
  # default in this shell otherwise.
  #
  # `z3`: the SMT backend for the dependent/refinement type system
  # (DESIGN.md's "Toward a dependent/refinement type system (SMT-backed,
  # planned)"). Linked dynamically via the `z3` crate against THIS
  # package, not the crate's own `bundled` feature (which compiles Z3
  # from source and pulls in a C++ toolchain this project doesn't
  # otherwise need) -- see that DESIGN.md section's "Build integration"
  # for why. `devenv.lock`'s pinned nixpkgs revision is what keeps proof
  # results deterministic across machines/time (a query provable under
  # one Z3 version can return `unknown` under another) -- load-bearing,
  # the same as the firtool/iverilog/verilator version pins below.
  packages = [pkgs.circt pkgs.iverilog pkgs.verilator pkgs.python3 pkgs.z3];

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
    # emit FIRRTL -> firtool -> Verilog -> a Verilog simulator against a
    # hand-written testbench at sim/<name>_tb.v. Backend is `iverilog`
    # (default) or `verilator`, an optional second positional arg — see
    # sim/README.md's "Verilator" section for what differs between them
    # (nothing about the FIRRTL/testbench content, only the invocation).
    # `-DSYNTHESIS` skips firtool's debug-only register-randomization
    # boilerplate, which uses an `automatic`-lifetime construct Icarus
    # doesn't support; skipping it is safe since our registers are all
    # properly reset regardless. `-lowering-options=disallowLocalVariables`
    # is the same Icarus-compatibility idea applied to a DIFFERENT source
    # of `automatic` locals: a cross-width intermediate (e.g. one `div`/
    # `rem` operand zero-extended to match the other's width) can lower to
    # an `automatic logic` declared INSIDE an `always` block instead of a
    # top-level `wire`, hit by examples/div_rem.tr's differing-width case
    # with tests/sim.rs's own (non---disable-opt) firtool invocation — see
    # sim/README.md. Harmless here too even when `--disable-opt` alone
    # already avoids it (confirmed empirically): same design, just a
    # `wire` instead of an `automatic logic` either way. Both flags are
    # kept for the `verilator` backend too, purely for parity with the
    # `iverilog` one (same generated Verilog either way) — Verilator
    # itself has no trouble with `automatic` locals or the debug
    # randomization block.
    simulate.exec = ''
      set -euo pipefail
      if [ -z "''${1:-}" ]; then
        echo "usage: simulate <name> [iverilog|verilator]   (expects examples/<name>.tr and sim/<name>_tb.v)" >&2
        exit 1
      fi
      backend="''${2:-iverilog}"
      if [ "$backend" != "iverilog" ] && [ "$backend" != "verilator" ]; then
        echo "simulate: unknown backend '$backend' (expected iverilog or verilator)" >&2
        exit 1
      fi

      # firtool/iverilog/verilator CLI behavior has already drifted once
      # for this project silently (a firtool bump started needing
      # `-format=fir` for stdin, with no clear signal until something
      # downstream broke in a confusing way) -- see sim/README.md and
      # TODO.md's "Simulation" section. `packages` above pins the exact
      # nixpkgs revision that resolves to these tool versions, but that
      # pin is only as good as someone noticing a future `devenv update`
      # moved it; this check makes a version drift a clear, immediate
      # failure right here instead of a confusing one three tools
      # downstream.
      want_firtool="firtool-1.147.0"
      got_firtool=$(firtool --version | grep -o 'firtool-[0-9.]*' || true)
      if [ "$got_firtool" != "$want_firtool" ]; then
        echo "simulate: expected $want_firtool, found '$got_firtool' -- firtool's CLI flags and output shape this project's scripts/tests assume may have changed; re-verify against sim/README.md before updating this pin" >&2
        exit 1
      fi
      if [ "$backend" = "iverilog" ]; then
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
      else
        want_verilator="5.050"
        got_verilator=$(verilator --version | sed -n 's/^Verilator \([0-9.]*\).*/\1/p')
        if [ "$got_verilator" != "$want_verilator" ]; then
          echo "simulate: expected verilator $want_verilator, found '$got_verilator'" >&2
          exit 1
        fi
      fi

      name="$1"
      repo_root=$(pwd)
      dir=$(mktemp -d)
      trap 'rm -rf "$dir"' EXIT
      cargo run -q -- "examples/$name.tr" --elaborate > "$dir/elaborated.tr"
      cargo run -q -- "$dir/elaborated.tr" --lower > "$dir/lowered.tr"
      cargo run -q -- "$dir/lowered.tr" --firrtl > "$dir/design.fir"
      firtool --disable-opt -lowering-options=disallowLocalVariables "$dir/design.fir" -o "$dir/design.v"

      # An `extmodule Name from "path.v"` declaration's `.v` implementation
      # is opaque data trace's own FIRRTL output never references at all
      # (see sim/README.md's "extmodule_tribuf_tb.v needs a second Verilog
      # source") -- both backends need it passed in directly, alongside the
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

      if [ "$backend" = "iverilog" ]; then
        iverilog -g2012 -DSYNTHESIS -o "$dir/sim" "sim/''${name}_tb.v" "$dir/design.v" "''${extmodule_srcs[@]:-}"
        vvp "$dir/sim"
      else
        # `--binary --timing`: Verilator elaborates the hand-written SV
        # testbench itself as the simulation's own top (not a separate
        # C++/DPI harness calling into a `--public`-exposed signal from
        # outside) and builds a standalone executable directly, the same
        # "just run it" shape `iverilog`+`vvp` already has. `--timing`
        # is what makes this mode accept the testbenches' own `#5`/`@
        # (posedge clock)` delays and `initial` blocks at all -- without
        # it Verilator (a synthesis-focused tool by default) rejects
        # most of that as unsupported. Because the testbench and DUT
        # elaborate as ONE design this way, `sim/subleq_tb.v`'s/
        # `sim/fifo_bridge_tb.v`'s hierarchical peeks and pokes
        # (`dut.pc`, `dut.__fifo_input_valid = ...`) are ordinary
        # intra-design SV hierarchical references, not a C++-boundary
        # crossing -- confirmed empirically (both run clean, with no
        # `--public`/`--public-flat-rw`, and no Verilator warning about
        # signal visibility). `--top-module` needs the testbench's own
        # module name, which this project's own convention (not
        # anything enforced) always matches its filename -- grepped
        # from the file itself rather than assumed, so a testbench that
        # ever broke this convention would fail loudly here instead of
        # silently picking the wrong top.
        top=$(grep -m1 -oE '^module +[A-Za-z_][A-Za-z0-9_]*' "sim/''${name}_tb.v" | awk '{print $2}')
        if [ -z "$top" ]; then
          echo "simulate: couldn't find a \`module <name>\` line in sim/''${name}_tb.v" >&2
          exit 1
        fi
        abs_extmodule_srcs=()
        for v in "''${extmodule_srcs[@]:-}"; do
          [ -n "$v" ] && abs_extmodule_srcs+=("$repo_root/$v")
        done
        (
          cd "$dir"
          verilator --binary --timing -DSYNTHESIS -Wno-fatal --top-module "$top" \
            design.v "$repo_root/sim/''${name}_tb.v" "''${abs_extmodule_srcs[@]:-}"
        )
        "$dir/obj_dir/V$top"
      fi
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
