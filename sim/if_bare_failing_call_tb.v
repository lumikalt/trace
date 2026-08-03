// Drives examples/if_bare_failing_call.tr: sweeps `a` through absent
// (0, `Classify` fails) and present (nonzero, succeeds) cases, checking
// `was_present` tracks the callee's own guard exactly -- the un-bound
// twin of sim/if_let_failing_call_tb.v.
`timescale 1ns/1ps

module if_bare_failing_call_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire was_present;
  integer fail = 0;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .was_present(was_present)
  );

  always #5 clock = ~clock;

  task check(input exp_present, input [127:0] label);
    begin
      if (was_present !== exp_present) begin
        $display("SIMULATION FAILED: %0s: was_present=%0d (expected %0d)",
                  label, was_present, exp_present);
        fail = 1;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    a = 0;
    @(posedge clock); #1;
    check(1'b0, "a=0, Classify fails");

    a = 7;
    @(posedge clock); #1;
    check(1'b1, "a=7, Classify succeeds");

    a = 0;
    @(posedge clock); #1;
    check(1'b0, "back to a=0, fails again");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
