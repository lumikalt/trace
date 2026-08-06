// Drives examples/call_max_min.tr (`Top`, whose rule computes `max(a, b,
// c)`/`min(a, b, c)`, all three real input ports) through real module
// ports -- proves `compile_max_min`'s left-folded `mux(gt/lt, next,
// acc)` chain picks the true extremum across three operands (not just
// two), including a tie between two of them.
`timescale 1ns/1ps

module call_max_min_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  reg [7:0] c = 0;
  wire [7:0] biggest;
  wire [7:0] smallest;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .c(c),
    .biggest(biggest),
    .smallest(smallest)
  );

  always #5 clock = ~clock;

  task check(
    input [7:0] av, input [7:0] bv, input [7:0] cv,
    input [7:0] biggest_expected, input [7:0] smallest_expected
  );
    begin
      a = av;
      b = bv;
      c = cv;
      @(posedge clock);
      #1;
      if (biggest !== biggest_expected || smallest !== smallest_expected) begin
        $display("SIMULATION FAILED: a=%0d b=%0d c=%0d biggest=%0d (expected %0d) smallest=%0d (expected %0d)",
          av, bv, cv, biggest, biggest_expected, smallest, smallest_expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'd5, 8'd10, 8'd3, 8'd10, 8'd3);   // b is the max, c is the min
    check(8'd200, 8'd10, 8'd3, 8'd200, 8'd3); // a is the max
    check(8'd7, 8'd7, 8'd7, 8'd7, 8'd7);      // all equal
    check(8'd0, 8'd255, 8'd128, 8'd255, 8'd0);

    $display("final: biggest=%0d smallest=%0d", biggest, smallest);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
