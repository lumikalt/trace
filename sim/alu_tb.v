// Drives examples/alu.tr through real module ports, like
// sim/accumulator_tb.v. a = 0xE3 (1110_0011) and b = 0x07 are chosen so
// a's high and low nibbles differ — a shl/shr mix-up, or a shift that
// drops the wrong end, produces a visibly different shl3/shr3 than the
// correct values, and the product exceeds either operand's own width so
// a truncated (rather than widening) multiply would also be caught.
`timescale 1ns/1ps

module alu_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [15:0] prod;
  wire [7:0] band, bor, bxor, shl3, shr3, nega, nota;
  wire [3:0] lo4;
  wire bit7;

  Alu dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .prod(prod),
    .band(band),
    .bor(bor),
    .bxor(bxor),
    .shl3(shl3),
    .shr3(shr3),
    .nega(nega),
    .nota(nota),
    .lo4(lo4),
    .bit7(bit7)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'hE3;
    b = 8'h07;

    repeat (2) @(posedge clock);
    #1;

    if (prod !== 16'h0635) begin
      $display("SIMULATION FAILED: prod=%0h, expected 0635 (227*7, full 16-bit product)", prod);
      $finish;
    end
    if (band !== 8'h03) begin
      $display("SIMULATION FAILED: band=%0h, expected 03", band);
      $finish;
    end
    if (bor !== 8'he7) begin
      $display("SIMULATION FAILED: bor=%0h, expected e7", bor);
      $finish;
    end
    if (bxor !== 8'he4) begin
      $display("SIMULATION FAILED: bxor=%0h, expected e4", bxor);
      $finish;
    end
    if (shl3 !== 8'h18) begin
      $display("SIMULATION FAILED: shl3=%0h, expected 18 (a<<3, truncated to 8 bits)", shl3);
      $finish;
    end
    if (shr3 !== 8'h1c) begin
      $display("SIMULATION FAILED: shr3=%0h, expected 1c (a>>3, zero-padded to 8 bits)", shr3);
      $finish;
    end
    if (nega !== 8'h1d) begin
      $display("SIMULATION FAILED: nega=%0h, expected 1d (-a mod 256)", nega);
      $finish;
    end
    if (nota !== 8'h1c) begin
      $display("SIMULATION FAILED: nota=%0h, expected 1c (~a)", nota);
      $finish;
    end
    if (lo4 !== 4'h3) begin
      $display("SIMULATION FAILED: lo4=%0h, expected 3 (a[3..0])", lo4);
      $finish;
    end
    if (bit7 !== 1'b1) begin
      $display("SIMULATION FAILED: bit7=%0b, expected 1 (a[7])", bit7);
      $finish;
    end

    $display(
      "final: prod=%0h band=%0h bor=%0h bxor=%0h shl3=%0h shr3=%0h nega=%0h nota=%0h lo4=%0h bit7=%0b",
      prod, band, bor, bxor, shl3, shr3, nega, nota, lo4, bit7
    );
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
