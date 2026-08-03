// Drives examples/if_let_fifo.tr: pushes one value into `f`, then checks
// that `consumer`'s `if let x = f.Deq[] { ... }` sees it exactly one
// cycle later (once, not sticking around) and sees "absent" on every
// other cycle -- proving the dequeue-enable this feature adds
// (`writes.rs`'s `compile_guard_unwrap_cond` recognizing a bare Deq,
// fifo.rs's `rule_fifo_ops` threading its `select`) is real hardware,
// not just a structural FIRRTL claim.
`timescale 1ns/1ps

module if_let_fifo_tb;
  reg clock = 0;
  reg reset = 1;
  reg push = 0;
  reg [7:0] push_val = 0;
  wire [7:0] result;
  wire was_present;
  integer fail = 0;

  FifoIfLet dut (
    .clock(clock),
    .reset(reset),
    .push(push),
    .push_val(push_val),
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

    // Fifo empty: consumer's `else` branch every cycle so far.
    @(posedge clock); #1;
    check(8'd0, 1'b0, "before any push");

    // Push 42 -- producer wins this cycle (fifo was empty), consumer
    // doesn't fire (mutual exclusion on the same fifo).
    push = 1; push_val = 42;
    @(posedge clock); #1;
    push = 0;
    check(8'd0, 1'b0, "the push cycle itself");

    // Next cycle: fifo now valid, consumer dequeues it.
    @(posedge clock); #1;
    check(8'd42, 1'b1, "one cycle after the push");

    // Drained: back to absent, and stays absent.
    @(posedge clock); #1;
    check(8'd0, 1'b0, "the cycle after the dequeue");
    @(posedge clock); #1;
    check(8'd0, 1'b0, "still absent");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
