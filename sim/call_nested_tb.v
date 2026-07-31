// Drives examples/call_nested.tr (`Top`, whose rule calls `Outer`, which
// itself calls `Inner`) through real module ports. x=10 stays well inside
// bits[8] (Inner: 11, doubled: 22); x=130 deliberately overflows in
// `Outer`'s own `* 2` (Inner: 131, 131*2=262, 262 mod 256 = 6) -- proving
// the nested call chain reuses ordinary modular arithmetic all the way
// through, not a wider intermediate the call boundary might otherwise hide.
`timescale 1ns/1ps

module call_nested_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'd10;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd22) begin
      $display("SIMULATION FAILED: result=%0d, expected 22 ((10+1)*2)", result);
      $finish;
    end

    x = 8'd130;
    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd6) begin
      $display("SIMULATION FAILED: result=%0d, expected 6 ((130+1)*2 mod 256)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
