// Drives examples/arith_shift.tr through real module ports. x = 0xE0
// (1110_0000, sign bit SET, -32 in two's complement) is chosen so `>>`
// and `>>>` genuinely disagree -- a value with its sign bit clear would
// make the two operators indistinguishable and prove nothing. Static
// (literal amount, 3) and dynamic (runtime `n`, driven with TWO
// different values across two cycles) are both checked, the same
// "genuinely read at runtime, not baked in at synthesis time" proof
// dynamic_shift_tb.v already establishes for `<<`/`>>`.
`timescale 1ns/1ps

module arith_shift_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  reg [2:0] n = 0;
  wire [7:0] shr_static, ashr_static, shr_dynamic, ashr_dynamic;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .n(n),
    .shr_static(shr_static),
    .ashr_static(ashr_static),
    .shr_dynamic(shr_dynamic),
    .ashr_dynamic(ashr_dynamic)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'hE0;
    n = 3'd3;

    repeat (1) @(posedge clock);
    #1;
    if (shr_static !== 8'h1c) begin
      $display("SIMULATION FAILED: shr_static=%0h, expected 1c (0xE0>>3)", shr_static);
      $finish;
    end
    if (ashr_static !== 8'hfc) begin
      $display("SIMULATION FAILED: ashr_static=%0h, expected fc (0xE0>>>3, sign-extended)", ashr_static);
      $finish;
    end
    if (shr_dynamic !== 8'h1c) begin
      $display("SIMULATION FAILED: n=3 shr_dynamic=%0h, expected 1c", shr_dynamic);
      $finish;
    end
    if (ashr_dynamic !== 8'hfc) begin
      $display("SIMULATION FAILED: n=3 ashr_dynamic=%0h, expected fc", ashr_dynamic);
      $finish;
    end

    n = 3'd5;
    repeat (1) @(posedge clock);
    #1;
    if (shr_dynamic !== 8'h07) begin
      $display("SIMULATION FAILED: n=5 shr_dynamic=%0h, expected 07 (0xE0>>5)", shr_dynamic);
      $finish;
    end
    if (ashr_dynamic !== 8'hff) begin
      $display("SIMULATION FAILED: n=5 ashr_dynamic=%0h, expected ff (0xE0>>>5, fully sign-extended)", ashr_dynamic);
      $finish;
    end

    $display("final: static shr=1c ashr=fc; n5 shr=%0h ashr=%0h", shr_dynamic, ashr_dynamic);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
