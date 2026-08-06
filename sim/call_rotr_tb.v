// Drives examples/call_rotr.tr (`Top`, whose rule computes `rotr(a,
// 3)`) through real module ports -- proves `compile_rotate`'s mirrored
// split (the `left: false` case, same function as `call_rotl_tb.v`)
// wraps the bottom 3 bits around to the top, fed the SAME input as the
// rotl testbench so the two results (0x0D vs 0x34) directly contrast
// the two directions.
`timescale 1ns/1ps

module call_rotr_tb;
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
    if (result !== 8'h34) begin // 0011_0100: bottom 3 bits (001) wrapped to top
      $display("SIMULATION FAILED: result=%b, expected 00110100", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
