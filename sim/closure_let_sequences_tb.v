// Drives examples/closure_let_sequences.tr -- proves a closure-shaped
// `let` inside a `<sequences>` rule, called once before a `tick` and
// once after, computes the right result on EACH side of the segment
// boundary: `r1 := f(x)` in the first cycle, `r2 := f(y)` in the second
// -- the real multi-cycle proof this example exists for, not just that
// it compiles.
`timescale 1ns/1ps

module closure_let_sequences_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  reg [7:0] y = 0;
  wire [7:0] r1;
  wire [7:0] r2;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .y(y),
    .r1(r1),
    .r2(r2)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'd10;
    y = 8'd20;

    // Segment 0 (r1 := x + 5) fires this edge.
    repeat (1) @(posedge clock);
    #1;
    if (r1 !== 8'd15) begin
      $display("SIMULATION FAILED: r1=%0d after segment 0, expected 15", r1);
      $finish;
    end

    // Segment 1 (r2 := y + 5) fires this edge.
    repeat (1) @(posedge clock);
    #1;
    if (r2 !== 8'd25) begin
      $display("SIMULATION FAILED: r2=%0d after segment 1, expected 25", r2);
      $finish;
    end

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
