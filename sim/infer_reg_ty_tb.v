// Drives examples/infer_reg_ty.tr through real module ports -- proves
// `reg acc = 8'd6` and `output hi = 16'hFF00` (both omitting `: ty`)
// actually declared a real bits[8]/bits[16], not just accepted syntax:
// `acc` resets to 6, adds `inc` each cycle, and `hi` holds its 0xFF00
// reset value forever (never written by any rule).
`timescale 1ns/1ps

module infer_reg_ty_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] inc = 0;
  wire [15:0] hi;
  wire [7:0] sum;

  InferRegTy dut (
    .clock(clock),
    .reset(reset),
    .inc(inc),
    .hi(hi),
    .sum(sum)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // One cycle past reset: `sum := acc` has now latched once, so `sum`
    // should read `acc`'s reset value (6), proving the inferred width
    // didn't silently reset to 0 or some other value.
    repeat (1) @(posedge clock);
    #1;
    if (sum !== 8'd6) begin
      $display("SIMULATION FAILED: sum=%0d, expected 6 (acc's inferred reset)", sum);
      $finish;
    end
    if (hi !== 16'hFF00) begin
      $display("SIMULATION FAILED: hi=%0h, expected ff00", hi);
      $finish;
    end

    // `sum` is itself a register reading `acc` combinationally, so it
    // trails `acc`'s own updates by one cycle -- three edges land two
    // full `+5` increments in `sum` (acc: 6->11->16->21, sum one behind).
    inc = 8'd5;
    repeat (3) @(posedge clock);
    #1;
    if (sum !== 8'd16) begin
      $display("SIMULATION FAILED: sum=%0d, expected 16 (one cycle behind acc's 6+2*5=16 -> 21)", sum);
      $finish;
    end
    if (hi !== 16'hFF00) begin
      $display("SIMULATION FAILED: hi=%0h, expected ff00 (never written)", hi);
      $finish;
    end

    $display("final: sum=%0d hi=%0h", sum, hi);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
