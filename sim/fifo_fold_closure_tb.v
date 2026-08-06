// Drives examples/fifo_fold_closure.tr through real module ports.
//
// Queues two items (10, 20) into the fifo while `drain` sits idle
// (`start = 0`), then pulses `start` once and waits generously --
// proving `while let x = f.Deq[] { total := combine(total, x) }`
// actually iterates MORE THAN ONCE per drain, folding a real two-
// argument closure across both items: `result` must be their SUM (30),
// not either value alone (which a single-iteration bug, or the
// depth > 1 `if let`/`while let` guard-gating bug this feature depends
// on being fixed, would both produce instead). A second, single-item
// drain afterward confirms `total` genuinely resets each restart
// (result becomes 5, not 35).
`timescale 1ns/1ps

module fifo_fold_closure_tb;
  reg clock = 0;
  reg reset = 1;
  reg push = 0;
  reg [7:0] push_val = 0;
  reg start = 0;
  wire [7:0] result;
  wire done;

  FifoFoldClosure dut (
    .clock(clock),
    .reset(reset),
    .push(push),
    .push_val(push_val),
    .start(start),
    .result(result),
    .done(done)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Queue two items while `drain` stays idle (start = 0).
    push = 1; push_val = 8'd10;
    @(posedge clock); #1;
    push_val = 8'd20;
    @(posedge clock); #1;
    push = 0;

    if (done !== 1'b0 || result !== 8'd0) begin
      $display("FAIL: before start, expected done=0 result=0, got done=%0d result=%0d",
                done, result);
      failed = 1;
    end

    // Trigger the drain -- both queued items fold into one sum.
    start = 1;
    @(posedge clock); #1;
    start = 0;

    repeat (10) @(posedge clock);
    #1;
    if (done !== 1'b1 || result !== 8'd30) begin
      $display("FAIL: after drain, expected done=1 result=30 (10+20), got done=%0d result=%0d",
                done, result);
      failed = 1;
    end

    // A second, single-item drain: `total` must reset to 0, not keep
    // accumulating from the first drain (35 would mean it didn't).
    push = 1; push_val = 8'd5;
    @(posedge clock); #1;
    push = 0;

    start = 1;
    @(posedge clock); #1;
    start = 0;

    repeat (10) @(posedge clock);
    #1;
    if (done !== 1'b1 || result !== 8'd5) begin
      $display("FAIL: second drain, expected done=1 result=5, got done=%0d result=%0d",
                done, result);
      failed = 1;
    end

    $display("final: result=%0d done=%0d", result, done);
    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
