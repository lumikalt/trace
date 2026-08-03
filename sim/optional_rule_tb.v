`timescale 1ns/1ps
module tb;
  reg clock = 0;
  reg reset = 1;
  reg step = 0;
  wire [3:0] count;
  OptionalRule dut(.clock(clock), .reset(reset), .step(step), .count(count));

  always #5 clock = ~clock;

  task tick;
    begin
      @(posedge clock);
      #1;
    end
  endtask

  task check(input [3:0] expected, input [127:0] label);
    begin
      if (count !== expected) begin
        $display("FAIL: %0s: expected count=%0d, got count=%0d", label, expected, count);
        $finish;
      end
    end
  endtask

  initial begin
    // Held high straight through reset: must NOT fire (no spurious
    // first-cycle edge, even though the port reads 1 from the start —
    // `__prev_step` resets to `1`, not `0`, specifically to suppress
    // this).
    step = 1;
    tick; tick;
    reset = 0;
    tick; tick; tick; tick;
    check(0, "held high through reset");

    // Still held high, no new edge: must not re-fire.
    tick; tick; tick;
    check(0, "held high, no edge");

    // A real 0->1 edge: fires exactly once.
    step = 0;
    tick;
    step = 1;
    tick;
    check(1, "first real edge");

    // Held high again: no re-fire.
    tick; tick; tick;
    check(1, "held high after first edge");

    // A second real edge.
    step = 0;
    tick;
    step = 1;
    tick;
    check(2, "second real edge");

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
