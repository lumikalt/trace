// Drives examples/call_writes.tr (`Top`, whose rule calls `Bump`, whose
// body both writes `v_out` AND returns a value assigned to `result`)
// through real module ports -- proves both halves of one call site
// land correctly: `result` from Bump's `return x + 1`, `v_out` from
// Bump's own internal `v_out := x`, independently threaded by
// `compile_callee_body` (the return value) and `callee_reg_write` (the
// write), not one computation feeding the other.
`timescale 1ns/1ps

module call_writes_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [7:0] result;
  wire [7:0] v_out;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .result(result),
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
    if (result !== 8'd21) begin
      $display("SIMULATION FAILED: result=%0d, expected 21 (20+1)", result);
      $finish;
    end
    if (v_out !== 8'd20) begin
      $display("SIMULATION FAILED: v_out=%0d, expected 20 (Bump's own write)", v_out);
      $finish;
    end

    $display("final: result=%0d v_out=%0d", result, v_out);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
