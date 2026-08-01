// Drives examples/race_value.tr through real firtool + Icarus, proving
// the value-producing `race[...]` form picks up the ACTUAL winner the
// scheduler decides, not some independent value-level tie-break of its
// own. A and B both take exactly one tick, so they'd be ready the SAME
// cycle -- but the cancellation mechanism (every racing spawn's own
// segments read its competitors' `done` directly, see DESIGN.md's "Race
// lowering") makes their own final segments mutually conflict, so the
// ordinary scheduler's own priority (declaration order: A was spawned
// first) picks exactly one of them to actually fire; the other never
// completes at all (`done` stays 0), rather than both landing the same
// cycle and needing `__race_value`'s own mux priority to break a real
// tie -- confirmed directly below by checking `done_hb` stays 0, not
// just that `result` came out right. Checked across THREE separate trigger
// cycles so a flaky result would show up as inconsistent, not just wrong.
`timescale 1ns/1ps

module race_value_tb;
  reg clock = 0;
  reg reset = 1;
  reg trigger = 0;
  wire [7:0] result;

  RaceValue dut (
    .clock(clock),
    .reset(reset),
    .trigger(trigger),
    .result(result)
  );

  always #5 clock = ~clock;

  reg failed;
  integer i;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    for (i = 0; i < 3; i = i + 1) begin
      @(posedge clock);
      #1;
      trigger = 1;
      @(posedge clock);
      #1;
      trigger = 0;

      repeat (3) @(posedge clock);
      #1;

      $display("trial %0d: result=%0d done_ha=%0d done_hb=%0d",
                i, result, dut.__done_pick_ha, dut.__done_pick_hb);
      if (result !== 8'd11) begin
        $display("FAIL: expected result == 11 (A's result), got %0d", result);
        failed = 1;
      end
      if (dut.__done_pick_hb !== 1'b0) begin
        $display("FAIL: expected B to never actually complete (scheduler priority should have blocked it) -- if this fires, result's correctness above no longer proves what this test claims");
        failed = 1;
      end
    end

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
