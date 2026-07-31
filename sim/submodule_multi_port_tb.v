// Drives examples/submodule_multi_port.tr through real module ports. The
// point of this design: `write_a` and `write_b` are two SEPARATE rules,
// each driving a different port of the same instance `x`. Under the old
// whole-instance conflict model, they'd conflict (both "write the
// instance") and only the higher-urgency one would ever fire, leaving
// `x.b` stuck at its 0 default forever. Per-port conflict precision lets
// both fire the same cycle, so `x.c` (and eventually `result`) reflects
// BOTH va and vb, not just va.
`timescale 1ns/1ps

module submodule_multi_port_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] va = 0;
  reg [7:0] vb = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .va(va),
    .vb(vb),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    va = 8'd5;
    vb = 8'd7;

    repeat (2) @(posedge clock);
    #1;
    if (result !== 8'd12) begin
      $display(
        "SIMULATION FAILED: result=%0d, expected 12 (5 + 7) -- if this is 5, \
write_b never fired (whole-instance conflict model regressed)",
        result
      );
      $finish;
    end

    $display("SIMULATION PASSED");
    $display("final: result=%0d", result);
    $finish;
  end
endmodule
