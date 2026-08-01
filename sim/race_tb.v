// Drives examples/race.tr through real firtool + Icarus, proving `race`
// actually cancels the loser rather than just letting it finish
// unobserved: Fast (1 tick) always beats Slow (2 ticks), and this
// checks the LOSER's own internal `done`/`result` registers directly by
// hierarchical path, not just the observable `out` port -- `out`
// alone can't tell "the loser was blocked" apart from "the loser
// finished too, but nobody happened to read its result", which is
// exactly the "losers run free" alternative design this test needs to
// rule out.
`timescale 1ns/1ps

module race_tb;
  reg clock = 0;
  reg reset = 1;
  reg trigger = 0;
  wire [7:0] out;

  Race dut (
    .clock(clock),
    .reset(reset),
    .trigger(trigger),
    .out(out)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Trigger cycle: pick_s0 fires, spawning both Fast and Slow.
    @(posedge clock);
    #1;
    trigger = 1;
    @(posedge clock);
    #1;
    trigger = 0;

    // Run well past Slow's own completion point (2 ticks) if it were
    // ever allowed to run unblocked.
    repeat (8) @(posedge clock);
    #1;

    $display("final: out=%0d done_hf=%0d done_hs=%0d result_hs=%0d",
              out, dut.__done_pick_hf, dut.__done_pick_hs, dut.__result_pick_hs);

    if (out !== 8'd2) begin
      $display("FAIL: expected out == 2 (Fast(1) == 1+1), got %0d", out);
      failed = 1;
    end
    if (dut.__done_pick_hf !== 1'b1) begin
      $display("FAIL: expected the winner (Fast) to have completed");
      failed = 1;
    end
    if (dut.__done_pick_hs !== 1'b0) begin
      $display("FAIL: Slow's done became 1 -- the loser was NOT cancelled, it ran to completion");
      failed = 1;
    end
    if (dut.__result_pick_hs !== 8'd0) begin
      $display("FAIL: Slow's result register was written (%0d) -- the loser's FSM kept running", dut.__result_pick_hs);
      failed = 1;
    end

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
