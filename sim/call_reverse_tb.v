// Drives examples/call_reverse.tr (`Top`, whose rule computes
// `reverse(a)`) through real module ports -- proves `compile_reverse`'s
// bit-by-bit `cat` chain actually flips the order (not, say, a no-op or
// a byte swap on a value too narrow for that to even mean anything
// different) with an asymmetric pattern that can't pass by coincidence.
`timescale 1ns/1ps

module call_reverse_tb;
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

  task check(input [7:0] v, input [7:0] expected);
    begin
      a = v;
      @(posedge clock);
      #1;
      if (result !== expected) begin
        $display("SIMULATION FAILED: a=%b result=%b, expected %b", v, result, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'h01, 8'h80); // lowest bit -> highest bit
    check(8'h03, 8'hC0); // low two bits -> high two bits

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
