// Drives examples/submodule_cond.tr (`Top`, which conditionally writes an
// instance port from inside if/else) through real module ports. Proves the
// nested write reaches the child as a mux, not a stale/dropped value: the
// `else` path must produce 0, not whatever `x` happened to be.
`timescale 1ns/1ps

module submodule_cond_tb;
  reg clock = 0;
  reg reset = 1;
  reg sel = 0;
  reg [7:0] x = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .sel(sel),
    .x(x),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    sel = 1;
    x = 8'd42;

    repeat (2) @(posedge clock);
    #1;
    if (result !== 8'd42) begin
      $display("SIMULATION FAILED: result=%0d, expected 42 (sel=1, x=42)", result);
      $finish;
    end

    sel = 0;
    x = 8'd99;

    repeat (2) @(posedge clock);
    #1;
    if (result !== 8'd0) begin
      $display("SIMULATION FAILED: result=%0d, expected 0 (sel=0)", result);
      $finish;
    end

    $display("SIMULATION PASSED");
    $display("final: result=%0d", result);
    $finish;
  end
endmodule
