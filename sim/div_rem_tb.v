// Drives examples/div_rem.tr through real module ports. a = 200 (fits
// bits[8], not bits[4]), b = 13 (fits bits[4]) -- deliberately different
// widths so the pad-vs-no-pad distinction in compile_binop's Div/Rem
// arms is actually exercised on both sides (dividend-is-wider and
// dividend-is-narrower), not just the equal-width case.
`timescale 1ns/1ps

module div_rem_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [3:0] b = 0;
  wire [7:0] q1, q2, r1, r2;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .q1(q1),
    .q2(q2),
    .r1(r1),
    .r2(r2)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'd200;
    b = 4'd13;

    repeat (1) @(posedge clock);
    #1;

    if (q1 !== 8'd15) begin
      $display("SIMULATION FAILED: q1=%0d, expected 15 (200/13)", q1);
      $finish;
    end
    if (q2 !== 8'd0) begin
      $display("SIMULATION FAILED: q2=%0d, expected 0 (13/200)", q2);
      $finish;
    end
    if (r1 !== 8'd5) begin
      $display("SIMULATION FAILED: r1=%0d, expected 5 (200%%13)", r1);
      $finish;
    end
    if (r2 !== 8'd13) begin
      $display("SIMULATION FAILED: r2=%0d, expected 13 (13%%200)", r2);
      $finish;
    end

    $display("final: q1=%0d q2=%0d r1=%0d r2=%0d", q1, q2, r1, r2);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
