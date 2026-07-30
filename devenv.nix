{ pkgs, lib, config, inputs, ... }:

{
  # https://devenv.sh/basics/
  env.GREET = "devenv";

  # https://devenv.sh/packages/
  packages = [ pkgs.git pkgs.circt pkgs.iverilog ];

  # https://devenv.sh/languages/
  languages.rust = {
    enable = true;
    channel = "nightly";
  };

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.hello.exec = ''
    echo hello from $GREET
  '';

  # Run the test suite with the address space capped at 4 GiB, so a
  # runaway loop aborts fast instead of stalling the machine until the
  # OOM killer steps in (ulimit -v takes KiB).
  scripts.t.exec = ''
    ulimit -v 4194304
    exec cargo test "$@"
  '';

  # Run a .tr file all the way to a passing/failing simulation: lower ->
  # emit FIRRTL -> firtool -> Verilog -> iverilog/vvp against a
  # hand-written testbench at sim/<name>_tb.v. `-DSYNTHESIS` skips
  # firtool's debug-only register-randomization boilerplate, which uses
  # an `automatic`-lifetime construct Icarus doesn't support; skipping
  # it is safe since our registers are all properly reset regardless.
  scripts.simulate.exec = ''
    set -euo pipefail
    if [ -z "''${1:-}" ]; then
      echo "usage: simulate <name>   (expects examples/<name>.tr and sim/<name>_tb.v)" >&2
      exit 1
    fi
    name="$1"
    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' EXIT
    cargo run -q -- "examples/$name.tr" --lower > "$dir/lowered.tr"
    cargo run -q -- "$dir/lowered.tr" --firrtl > "$dir/design.fir"
    firtool --disable-opt "$dir/design.fir" -o "$dir/design.v"
    iverilog -g2012 -DSYNTHESIS -o "$dir/sim" "sim/''${name}_tb.v" "$dir/design.v"
    vvp "$dir/sim"
  '';

  # https://devenv.sh/basics/
  enterShell = ''
    hello         # Run scripts directly
    git --version # Use packages
  '';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    t
  '';

  # https://devenv.sh/git-hooks/
  git-hooks.hooks.rustfmt.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
