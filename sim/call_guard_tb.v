// Drives examples/call_guard.tr (`Top`, whose rule calls `Classify`,
// a callee whose only fail source is a bare top-level guard) through
// real module ports. Proves the fold actually reaches the caller's own
// guard, correctly substituted against the call site's argument (`a`,
// not `Classify`'s own parameter name `x`): the rule must NOT fire
// while a == 0 (result stays 0, held), then must fire and inline
// Classify's return value once a != 0, then must go back to holding
// once a returns to 0 (the guard blocks every cycle it's false, not
// just the first one).
`timescale 1ns/1ps

module call_guard_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    a = 8'd0;
    @(posedge clock); #1;
    if (result !== 8'd0) begin
      $display("SIMULATION FAILED: result=%0d, expected 0 (a=0 should not fire Classify's guard)", result);
      $finish;
    end

    a = 8'd7;
    @(posedge clock); #1;
    if (result !== 8'd7) begin
      $display("SIMULATION FAILED: result=%0d, expected 7 after a=7", result);
      $finish;
    end

    a = 8'd0;
    @(posedge clock); #1;
    if (result !== 8'd7) begin
      $display("SIMULATION FAILED: result=%0d, expected result to HOLD at 7 (a=0 should not fire)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
