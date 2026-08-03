// Drives examples/while_let_drain.tr through real module ports.
//
// Proves `while let`'s multi-cycle lowering actually iterates once per
// cycle in real hardware, gated on a real `opt_valid` register (not
// just that firtool accepts the emitted `if let`-as-self-loop text):
// holds `x` at 5, 0, then 12 in turn and checks `iters` settles at
// exactly `x` each time. `x=0` pins the zero-iteration edge case (the
// loop's own Option is already absent the first time it's checked).
`timescale 1ns/1ps

module while_let_drain_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] iters;

  WhileLetDrain dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .iters(iters)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    x = 8'd5;
    repeat (20) @(posedge clock);
    #1;
    if (iters !== 8'd5) begin
      $display("FAIL: x=5 expected iters=5, got %0d", iters);
      failed = 1;
    end

    x = 8'd0;
    repeat (20) @(posedge clock);
    #1;
    if (iters !== 8'd0) begin
      $display("FAIL: x=0 expected iters=0, got %0d", iters);
      failed = 1;
    end

    x = 8'd12;
    repeat (30) @(posedge clock);
    #1;
    if (iters !== 8'd12) begin
      $display("FAIL: x=12 expected iters=12, got %0d", iters);
      failed = 1;
    end

    $display("final: iters=%0d", iters);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
