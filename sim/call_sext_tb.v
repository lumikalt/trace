// Drives examples/call_sext.tr (`Top`, whose rule computes `sext(a,
// 16)`) through real module ports -- proves `compile_sext`'s
// `asUInt(pad(asSInt(a), 16))` fills the new high byte by REPLICATING
// the top bit, not zeroing it (that's `call_zext_tb.v`, fed the
// identical input value to make the contrast direct: same `a`, opposite
// high byte).
`timescale 1ns/1ps

module call_sext_tb;
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
    if (result !== 16'hFF80) begin
      $display("SIMULATION FAILED: result=%0h, expected ff80 (sign-extended)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
