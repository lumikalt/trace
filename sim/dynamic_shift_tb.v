// Drives examples/dynamic_shift.tr through real module ports. x = 0xE3
// (1110_0011, high/low nibbles differ, same value alu_tb.v's static
// shift already proved 0xE3<<3=0x18 / 0xE3>>3=0x1c against) stays fixed
// while `n` changes between TWO different values across two cycles --
// proving the shift amount is genuinely read at runtime, not a constant
// baked in at synthesis time the way a static `<<3` would be.
`timescale 1ns/1ps

module dynamic_shift_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  reg [2:0] n = 0;
  wire [7:0] shl_out, shr_out;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .n(n),
    .shl_out(shl_out),
    .shr_out(shr_out)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'hE3;
    n = 3'd3;

    repeat (1) @(posedge clock);
    #1;
    if (shl_out !== 8'h18) begin
      $display("SIMULATION FAILED: n=3 shl_out=%0h, expected 18 (0xE3<<3, truncated)", shl_out);
      $finish;
    end
    if (shr_out !== 8'h1c) begin
      $display("SIMULATION FAILED: n=3 shr_out=%0h, expected 1c (0xE3>>3)", shr_out);
      $finish;
    end

    n = 3'd5;
    repeat (1) @(posedge clock);
    #1;
    if (shl_out !== 8'h60) begin
      $display("SIMULATION FAILED: n=5 shl_out=%0h, expected 60 (0xE3<<5, truncated)", shl_out);
      $finish;
    end
    if (shr_out !== 8'h07) begin
      $display("SIMULATION FAILED: n=5 shr_out=%0h, expected 07 (0xE3>>5)", shr_out);
      $finish;
    end

    $display("final: n3 shl=18 shr=1c; n5 shl=%0h shr=%0h", shl_out, shr_out);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
