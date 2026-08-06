// Drives examples/call_popcount.tr (`Top`, whose rule computes
// `popcount(a)`) through real module ports -- proves `compile_popcount`'s
// `add` chain over each bit counts correctly, across the boundary cases
// (all-zero, all-one) and a mixed pattern.
`timescale 1ns/1ps

module call_popcount_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [3:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .result(result)
  );

  always #5 clock = ~clock;

  task check(input [7:0] v, input [3:0] expected);
    begin
      a = v;
      @(posedge clock);
      #1;
      if (result !== expected) begin
        $display("SIMULATION FAILED: a=%b result=%0d, expected %0d", v, result, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'h00, 0); // none set
    check(8'hFF, 8); // all set
    check(8'h0F, 4); // low nibble
    check(8'hA5, 4); // mixed pattern (10100101)

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
