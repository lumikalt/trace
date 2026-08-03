// Drives examples/if_let_failing_call.tr: sweeps `a` through absent
// (0, `Classify` fails) and present (nonzero, `Classify` succeeds)
// cases, checking `result`/`was_present` track the callee's own guard
// exactly -- proving the mux-select this feature adds (`writes.rs`'s
// `compile_guard_unwrap_cond` recognizing a bare failing call, reusing
// `calls.rs`'s `callee_fail_cond` AS-IS, not negated) is real hardware,
// not just a structural FIRRTL claim.
`timescale 1ns/1ps

module if_let_failing_call_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  wire [7:0] result;
  wire was_present;
  integer fail = 0;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .result(result),
    .was_present(was_present)
  );

  always #5 clock = ~clock;

  task check(input [7:0] exp_result, input exp_present, input [127:0] label);
    begin
      if (result !== exp_result || was_present !== exp_present) begin
        $display("SIMULATION FAILED: %0s: result=%0d was_present=%0d (expected result=%0d was_present=%0d)",
                  label, result, was_present, exp_result, exp_present);
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
    check(8'd0, 1'b0, "a=0, Classify fails");

    a = 7;
    @(posedge clock); #1;
    check(8'd7, 1'b1, "a=7, Classify succeeds");

    a = 0;
    @(posedge clock); #1;
    check(8'd0, 1'b0, "back to a=0, fails again");

    a = 200;
    @(posedge clock); #1;
    check(8'd200, 1'b1, "a=200, succeeds at a wide value too");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
