// Drives examples/fifo_depth.tr (a depth-3 fifo) through real module
// ports.
//
// Fills the fifo to full (3 pushes), exercises a full-buffer combined
// Enq+Deq pass-through (`rule pushpop`), drains to empty, then refills
// and drains a second time -- the second fill's writes land at tail
// positions 1, 2, 0, wrapping the internal `head`/`tail` pointers past
// the top slot index, the same non-power-of-2 wraparound stress case
// hand-verified (via a raw FIRRTL circuit + firtool + Icarus) before
// this codegen was written. Expects the dequeued values to come back
// in exact FIFO order: 10, 20, 30, 40, 50, 60, 70.
`timescale 1ns/1ps

module fifo_depth_tb;
  reg clock = 0;
  reg reset = 1;
  reg want_push = 0;
  reg want_pop = 0;
  reg [7:0] push_data = 0;
  wire [7:0] last_deq;

  FifoDepth dut (
    .clock(clock),
    .reset(reset),
    .want_push(want_push),
    .want_pop(want_pop),
    .push_data(push_data),
    .last_deq(last_deq)
  );

  always #5 clock = ~clock;

  reg failed;

  task push(input [7:0] data);
    begin
      want_push = 1;
      want_pop = 0;
      push_data = data;
      @(posedge clock);
      #1;
      want_push = 0;
    end
  endtask

  task pop_and_expect(input [7:0] expected);
    begin
      want_push = 0;
      want_pop = 1;
      @(posedge clock);
      #1;
      want_pop = 0;
      if (last_deq !== expected) begin
        $display("FAIL: expected last_deq == %0d, got %0d", expected, last_deq);
        failed = 1;
      end
    end
  endtask

  task pushpop_and_expect(input [7:0] data, input [7:0] expected);
    begin
      want_push = 1;
      want_pop = 1;
      push_data = data;
      @(posedge clock);
      #1;
      want_push = 0;
      want_pop = 0;
      if (last_deq !== expected) begin
        $display("FAIL: expected last_deq == %0d, got %0d", expected, last_deq);
        failed = 1;
      end
    end
  endtask

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Fill to full: 10, 20, 30.
    push(8'd10);
    push(8'd20);
    push(8'd30);

    // Idle cycle on a full buffer: nothing should change.
    @(posedge clock);
    #1;

    // Full-buffer combined Enq+Deq pass-through: reads the oldest
    // (10) while writing 40 in; fifo stays full (count unchanged).
    pushpop_and_expect(8'd40, 8'd10);

    // Drain the rest in order.
    pop_and_expect(8'd20);
    pop_and_expect(8'd30);
    pop_and_expect(8'd40);

    // Refill and drain again -- forces the internal pointers to wrap
    // past the top slot index (non-power-of-2 depth).
    push(8'd50);
    push(8'd60);
    pop_and_expect(8'd50);
    push(8'd70);
    pop_and_expect(8'd60);
    pop_and_expect(8'd70);

    $display("final: last_deq=%0d", last_deq);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
