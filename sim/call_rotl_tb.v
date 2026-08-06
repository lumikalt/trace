// Drives examples/call_rotl.tr (`Top`, whose rule computes `rotl(a,
// 3)`) through real module ports -- proves `compile_rotate`'s `cat` of
// the two static bit-slices actually rotates (wraps the top 3 bits
// around to the bottom) rather than just shifting them off the top.
`timescale 1ns/1ps

module call_rotl_tb;
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
    a = 8'hA1; // 1010_0001

    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'h0D) begin // 0000_1101: top 3 bits (101) wrapped to bottom
      $display("SIMULATION FAILED: result=%b, expected 00001101", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
