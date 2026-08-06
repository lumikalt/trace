// Drives examples/call_zext.tr (`Top`, whose rule computes `zext(a,
// 16)`) through real module ports -- proves `compile_zext`'s `pad(a,
// 16)` fills the new high byte with ZERO, by feeding a value whose top
// bit is set (the case a sign-extend would get wrong).
`timescale 1ns/1ps

module call_zext_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [15:0] result;

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
    a = 8'h80;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 16'h0080) begin
      $display("SIMULATION FAILED: result=%0h, expected 0080 (zero-extended)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
