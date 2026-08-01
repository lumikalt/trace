// Drives examples/fifo_passthrough.tr through real module ports.
//
// Proves a rule enqueueing AND dequeueing the SAME fifo genuinely
// works, continuously, not just once: after seeding the fifo with 10,
// `last_out` must increment every single cycle (10, 11, 12, ...) with
// no stall or dead cycle -- confirming `step`'s combined guard is
// `valid == 1` alone, not the always-false AND of Deq's and Enq's own
// individual guards, and that Deq's value is the OLD (pre-edge) data
// while Enq's value lands for the NEXT cycle.
`timescale 1ns/1ps

module fifo_passthrough_tb;
  reg clock = 0;
  reg reset = 1;
  reg seed = 0;
  reg [7:0] seed_value = 0;
  wire [7:0] last_out;

  FifoPassthrough dut (
    .clock(clock),
    .reset(reset),
    .seed(seed),
    .seed_value(seed_value),
    .last_out(last_out)
  );

  always #5 clock = ~clock;

  reg failed;
  integer i;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Seed the (empty) fifo with 10 for one cycle.
    seed = 1;
    seed_value = 8'd10;
    @(posedge clock);
    #1;
    seed = 0;

    // `step` now fires every cycle: last_out should read 10, 11, 12,
    // 13, 14 across five consecutive cycles, no gaps or repeats.
    for (i = 0; i < 5; i = i + 1) begin
      @(posedge clock);
      #1;
      if (last_out !== 8'd10 + i[7:0]) begin
        $display("FAIL: cycle %0d expected last_out == %0d, got %0d", i, 10 + i, last_out);
        failed = 1;
      end
    end

    $display("final: last_out=%0d", last_out);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
