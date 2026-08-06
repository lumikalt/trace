// Drives examples/call_max_min_mixed_width.tr (`Top`, `a : [4]`, `b :
// [8]` -- genuinely DIFFERENT widths, unlike every other max/min
// testbench in this suite) through real module ports. Proves two things
// together: FIRRTL's `gt`/`lt` compare correctly across differently-sized
// `UInt` operands (reasoned about from the FIRRTL spec, not otherwise
// exercised here), and `min`'s own trailing `bits(..., 3, 0)` truncation
// is correct, not just present -- `min(a, b)`'s declared width is `a`'s
// own narrower [4], so a bug in that truncation (or in `types.rs`'s own
// "min takes the narrower operand's width" rule) would show up here as
// a wrong value, not just a wrong bit count.
`timescale 1ns/1ps

module call_max_min_mixed_width_tb;
  reg clock = 0;
  reg reset = 1;
  reg [3:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] biggest;
  wire [3:0] smallest;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .biggest(biggest),
    .smallest(smallest)
  );

  always #5 clock = ~clock;

  task check(
    input [3:0] av, input [7:0] bv,
    input [7:0] biggest_expected, input [3:0] smallest_expected
  );
    begin
      a = av;
      b = bv;
      @(posedge clock);
      #1;
      if (biggest !== biggest_expected || smallest !== smallest_expected) begin
        $display("SIMULATION FAILED: a=%0d b=%0d biggest=%0d (expected %0d) smallest=%0d (expected %0d)",
          av, bv, biggest, biggest_expected, smallest, smallest_expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(4'd5, 8'd200, 8'd200, 4'd5);  // b (numerically) is the max, a is the min
    check(4'd15, 8'd3, 8'd15, 4'd3);    // a (max nibble value) is the max, b is the min
    check(4'd0, 8'd0, 8'd0, 4'd0);
    check(4'd15, 8'd255, 8'd255, 4'd15);

    $display("final: biggest=%0d smallest=%0d", biggest, smallest);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
