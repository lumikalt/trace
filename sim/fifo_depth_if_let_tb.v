// Drives examples/fifo_depth_if_let.tr -- proves `emit_fifo_depth_n`
// (depth > 1 fifo emission) gates its head/count update on the if-let's
// own guard (`FifoSelect::Cond`) instead of updating unconditionally
// every cycle `consumer` fires. Under the bug this regresses, `count`
// decrements every idle cycle even with nothing enqueued, wrapping past
// zero and making `was_present` spuriously stick at 1 with garbage
// `last` data -- so the idle checks both before the first push and
// after the pushed item drains are the load-bearing assertions here,
// not just the single push-then-pop round trip.
`timescale 1ns/1ps

module fifo_depth_if_let_tb;
  reg clock = 0;
  reg reset = 1;
  reg push = 0;
  reg [7:0] push_val = 0;
  wire [7:0] last;
  wire was_present;
  integer fail = 0;

  FifoDepthIfLet dut (
    .clock(clock),
    .reset(reset),
    .push(push),
    .push_val(push_val),
    .last(last),
    .was_present(was_present)
  );

  always #5 clock = ~clock;

  task check(input [7:0] exp_last, input exp_present, input [127:0] label);
    begin
      if (last !== exp_last || was_present !== exp_present) begin
        $display("SIMULATION FAILED: %0s: last=%0d was_present=%0d (expected last=%0d was_present=%0d)",
                  label, last, was_present, exp_last, exp_present);
        fail = 1;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Idle, fifo empty: `was_present` must stay 0 across several
    // cycles -- the bug makes `count` wrap to nonzero after just one.
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle 1, before any push");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle 2, before any push");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle 3, before any push");

    // Push 42 while empty -- lands next edge.
    push = 1; push_val = 42;
    @(posedge clock); #1;
    push = 0;
    check(8'd0, 1'b0, "the push cycle itself");

    // Next cycle: fifo now has one item, consumer dequeues it.
    @(posedge clock); #1;
    check(8'd42, 1'b1, "one cycle after the push");

    // Drained again: back to absent, and stays absent over several
    // more idle cycles.
    @(posedge clock); #1;
    check(8'd0, 1'b0, "the cycle after the dequeue");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle after drain 1");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle after drain 2");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "idle after drain 3");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
