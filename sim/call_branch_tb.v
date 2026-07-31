// Drives examples/call_branch.tr (`Top`, whose rule calls the pure
// helper `Max`, whose body branches on `if`/`else`) through real module
// ports -- proves the mux `compile_callee_body` builds for a branching
// return picks the right side, on both a=200,b=100 (else-branch: b > a)
// and 200,50 (then-branch: a > b), not just whichever branch happens to
// come first in the source.
`timescale 1ns/1ps

module call_branch_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'd100;
    b = 8'd200;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd200) begin
      $display("SIMULATION FAILED: result=%0d, expected 200 (else branch: b > a)", result);
      $finish;
    end

    a = 8'd200;
    b = 8'd50;
    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd200) begin
      $display("SIMULATION FAILED: result=%0d, expected 200 (then branch: a > b)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
