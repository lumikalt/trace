// Drives examples/call_nested_writes.tr through real module ports --
// `compute` calls `Outer` (bare statement), which calls `Inner` (also a
// bare statement), which writes `v_out`. Proves a state write threads
// through TWO levels of nested calls, not just one, landing at
// v_out = a + 1.
`timescale 1ns/1ps

module call_nested_writes_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [7:0] v_out;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .v_out(v_out)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'd20;

    repeat (1) @(posedge clock);
    #1;
    if (v_out !== 8'd21) begin
      $display("SIMULATION FAILED: v_out=%0d, expected 21 (20+1, via Outer->Inner)", v_out);
      $finish;
    end

    $display("final: v_out=%0d", v_out);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
