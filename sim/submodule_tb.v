// Drives examples/submodule.tr (`Top`, which `inst`-instantiates `Adder`)
// through real module ports — no hierarchical peek/poke. Proves the
// two-cycle latency a submodule chain adds: `Adder`'s own `output sum` is
// one cycle behind `a`/`b` (same as any `output`, see accumulator_tb.v),
// and `Top`'s `result := adder.sum` is a *second* register hop behind
// that, so `result` reflects `x`/`y` two cycles after they are driven,
// not one.
`timescale 1ns/1ps

module submodule_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  reg [7:0] y = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .y(y),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'd5;
    y = 8'd7;

    repeat (2) @(posedge clock);
    #1;
    if (result !== 8'd12) begin
      $display("SIMULATION FAILED: result=%0d, expected 12 (5 + 7)", result);
      $finish;
    end

    x = 8'd20;
    y = 8'd30;
    repeat (2) @(posedge clock);
    #1;
    if (result !== 8'd50) begin
      $display("SIMULATION FAILED: result=%0d, expected 50 (20 + 30)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
