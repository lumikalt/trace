// Drives examples/call_backtick.tr (`Top`, whose rule computes
// `a `Avg` b` and `a `max` b`) through real module ports -- proves the
// backtick-infix sugar reaches the same compiled circuit an ordinary
// `Avg(a, b)`/`max(a, b)` call would, for both a user function and a
// builtin.
`timescale 1ns/1ps

module call_backtick_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] avg;
  wire [7:0] biggest;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .avg(avg),
    .biggest(biggest)
  );

  always #5 clock = ~clock;

  task check(
    input [7:0] av, input [7:0] bv,
    input [7:0] avg_expected, input [7:0] biggest_expected
  );
    begin
      a = av;
      b = bv;
      @(posedge clock);
      #1;
      if (avg !== avg_expected || biggest !== biggest_expected) begin
        $display("SIMULATION FAILED: a=%0d b=%0d avg=%0d (expected %0d) biggest=%0d (expected %0d)",
          av, bv, avg, avg_expected, biggest, biggest_expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'd20, 8'd30, 8'd25, 8'd30);
    check(8'd200, 8'd100, 8'd22, 8'd200); // Avg's sum overflows [8]: 300 mod 256 = 44, >>1 = 22
    check(8'd7, 8'd7, 8'd7, 8'd7);

    $display("final: avg=%0d biggest=%0d", avg, biggest);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
