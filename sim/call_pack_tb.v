// Drives examples/call_pack.tr (`Top`, whose rule computes `pack(a, b)`)
// through real module ports -- proves `compile_pack`'s `cat(a, b)`
// really does put `a` in the HIGH byte, not the low one, by feeding
// distinguishable values for each half.
`timescale 1ns/1ps

module call_pack_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [15:0] result;

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
    a = 8'hAA;
    b = 8'hBB;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 16'hAABB) begin
      $display("SIMULATION FAILED: result=%0h, expected aabb (a in the high byte)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
