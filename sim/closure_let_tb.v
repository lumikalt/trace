// Drives examples/closure_let.tr -- proves a `let`-bound closure (`f`),
// called twice with different arguments, computes the right,
// independent result each time through real hardware: `r1 := f(x)`
// (x + 5) and `r2 := f(x + 1)` ((x + 1) + 5), including wrapping past
// `bits[8]` -- not just that it compiles and firtool accepts it.
`timescale 1ns/1ps

module closure_let_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] r1;
  wire [7:0] r2;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .r1(r1),
    .r2(r2)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'd10;

    repeat (1) @(posedge clock);
    #1;
    if (r1 !== 8'd15 || r2 !== 8'd16) begin
      $display("SIMULATION FAILED: r1=%0d r2=%0d, expected r1=15 r2=16", r1, r2);
      $finish;
    end

    // Wraps past bits[8] -- proves the closure's own substituted call
    // isn't silently widened anywhere along the way.
    x = 8'd251;

    repeat (1) @(posedge clock);
    #1;
    if (r1 !== 8'd0 || r2 !== 8'd1) begin
      $display("SIMULATION FAILED: r1=%0d r2=%0d, expected r1=0 r2=1 (wraps past bits[8])", r1, r2);
      $finish;
    end

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
