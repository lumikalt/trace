// Drives examples/call_trunc.tr (`Top`, whose rule computes `trunc(a, 8)`)
// through real module ports -- proves `compile_trunc`'s `bits(a, 7, 0)`
// actually keeps the LOW 8 bits, not the high ones, by feeding a value
// whose low and high bytes differ.
`timescale 1ns/1ps

module call_trunc_tb;
  reg clock = 0;
  reg reset = 1;
  reg [15:0] a = 0;
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
    a = 16'hBEEF;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'hEF) begin
      $display("SIMULATION FAILED: result=%0h, expected ef (low byte of beef)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
