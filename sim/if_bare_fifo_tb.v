// Drives examples/if_bare_fifo.tr: pushes one value into `f`, then
// checks that `consumer`'s bare `if f.Deq[] { ... }` sees it exactly one
// cycle later (once, not sticking around), absent on every other cycle
// -- the same shape sim/if_let_fifo_tb.v proves for the bound-name
// (`if let`) form, confirming the un-bound form's dequeue-enable is
// identical real hardware, not just a structural FIRRTL claim.
`timescale 1ns/1ps

module if_bare_fifo_tb;
  reg clock = 0;
  reg reset = 1;
  reg push = 0;
  reg [7:0] push_val = 0;
  wire was_present;
  integer fail = 0;

  Top dut (
    .clock(clock),
    .reset(reset),
    .push(push),
    .push_val(push_val),
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

    @(posedge clock); #1;
    check(1'b0, "before any push");

    push = 1; push_val = 42;
    @(posedge clock); #1;
    push = 0;
    check(1'b0, "the push cycle itself");

    @(posedge clock); #1;
    check(1'b1, "one cycle after the push");

    @(posedge clock); #1;
    check(1'b0, "drained, back to absent");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
