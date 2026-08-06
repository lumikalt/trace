// Drives examples/call_rotr_dynamic.tr the same way call_rotl_dynamic_tb.v
// drives its rotl counterpart -- see that file's own header for why the
// >= width amounts matter. `rotr`'s dynamic path is the simpler of the
// two (`dshr` by `n mod w` directly, no `sub` needed), fed the SAME
// (a, n) pairs so the two results (e.g. 0x34 vs 0x0D for n=3) directly
// contrast the two directions, same as the constant-amount testbenches.
`timescale 1ns/1ps

module call_rotr_dynamic_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [3:0] n = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .n(n),
    .result(result)
  );

  always #5 clock = ~clock;

  task check(input [7:0] av, input [3:0] nv, input [7:0] expected);
    begin
      a = av;
      n = nv;
      @(posedge clock);
      #1;
      if (result !== expected) begin
        $display("SIMULATION FAILED: a=%b n=%0d result=%b, expected %b", av, nv, result, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'hA1, 0, 8'hA1);
    check(8'hA1, 3, 8'h34);
    check(8'hA1, 7, 8'h43);
    check(8'hA1, 8, 8'hA1);  // amount == width -- reduces to 0
    check(8'hA1, 11, 8'h34); // amount > width -- reduces to 3
    check(8'hFF, 4, 8'hFF);
    check(8'h01, 1, 8'h80);

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
