// Drives examples/branch_fifo_deq.tr: pushes one value routed to
// `out_a`, then another routed to `out_b`, and checks each lands on its
// own output while the OTHER output holds its prior value -- proving the
// two branch-nested `Deq[]`s (one in `then`, one in `else` of the same
// `if`) really are mutually exclusive real hardware, not just a
// structural FIRRTL claim that happens to type-check.
`timescale 1ns/1ps

module branch_fifo_deq_tb;
  reg clock = 0;
  reg reset = 1;
  reg push = 0;
  reg [7:0] push_val = 0;
  reg route = 0;
  wire [7:0] out_a;
  wire [7:0] out_b;
  integer fail = 0;

  Top dut (
    .clock(clock),
    .reset(reset),
    .push(push),
    .push_val(push_val),
    .route(route),
    .out_a(out_a),
    .out_b(out_b)
  );

  always #5 clock = ~clock;

  task check(input [7:0] a_exp, input [7:0] b_exp, input [127:0] label);
    begin
      if (out_a !== a_exp || out_b !== b_exp) begin
        $display("SIMULATION FAILED: %0s: out_a=%0d out_b=%0d (expected %0d,%0d)",
                  label, out_a, out_b, a_exp, b_exp);
        fail = 1;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    @(posedge clock); #1;
    check(8'd0, 8'd0, "before any push");

    push = 1; push_val = 42; route = 1;
    @(posedge clock); #1;
    push = 0;
    check(8'd0, 8'd0, "the push cycle itself");

    @(posedge clock); #1;
    check(8'd42, 8'd0, "one cycle after push, routed to out_a");

    push = 1; push_val = 7; route = 0;
    @(posedge clock); #1;
    push = 0;
    @(posedge clock); #1;
    check(8'd42, 8'd7, "one cycle after push, routed to out_b -- out_a still holds 42");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
