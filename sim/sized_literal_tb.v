// Drives examples/sized_literal.tr (`Top`, whose rule uses the sized
// literals `8'd6` and `8'd3`) through real module ports -- proves a
// sized literal both widens correctly against a wider operand in
// arithmetic (`x + 8'd6`) and works as a bit-select bound (`x[8'd3]`).
`timescale 1ns/1ps

module sized_literal_tb;
  reg clock = 0;
  reg reset = 1;
  reg [15:0] x = 0;
  wire [15:0] result;
  wire bit3;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .result(result),
    .bit3(bit3)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 16'b0000_0000_0000_1000; // bit 3 set, rest clear

    repeat (1) @(posedge clock);
    #1;
    if (result !== 16'd14) begin
      $display("SIMULATION FAILED: result=%0d, expected 14 (8+6)", result);
      $finish;
    end
    if (bit3 !== 1'b1) begin
      $display("SIMULATION FAILED: bit3=%0b, expected 1", bit3);
      $finish;
    end

    $display("final: result=%0d bit3=%0b", result, bit3);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
