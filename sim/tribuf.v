// The real implementation behind examples/extmodule_tribuf.tr's
// `extmodule TriBuf from "tribuf.v"` declaration. trace's FIRRTL output
// never references this file (see ast::Item::ExtModule's doc comment) —
// it's supplied to iverilog directly, alongside the generated design, by
// whoever is simulating (here, tests/sim.rs's `simulate_with_blackbox`).
//
// A plain combinational tri-state buffer: drives `data` onto `pad` when
// `enable`, floats otherwise; always reads `pad` back onto `sensed`
// regardless of who (if anyone) is driving it.
module TriBuf(
  input        enable,
  input  [7:0] data,
  output [7:0] sensed,
  inout  [7:0] pad
);
  assign pad = enable ? data : 8'bz;
  assign sensed = pad;
endmodule
