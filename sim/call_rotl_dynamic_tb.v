// Drives examples/call_rotl_dynamic.tr (`Top`, whose rule computes
// `rotl(a, n)` with `n` a real input port, not a literal) through real
// module ports -- proves `compile_rotate`'s dynamic path (`dup =
// cat(a, a)`, shifted by `w - (n mod w)` via `dshr`) rotates correctly
// across several runtime amounts, including ones AT and PAST the
// value's own width (8), which only the `rem`-based modulo reduction
// gets right -- the constant-amount path (`call_rotl_tb.v`) never
// exercises that reduction at all, since a constant amount is reduced
// once at compile time in Rust instead.
`timescale 1ns/1ps

module call_rotl_dynamic_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [3:0] n = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .n(n),
    .result(result)
  );

  always #5 clock = ~clock;

  task check(input [7:0] av, input [3:0] nv, input [7:0] expected);
    begin
      a = av;
      n = nv;
      @(posedge clock);
      #1;
      if (result !== expected) begin
        $display("SIMULATION FAILED: a=%b n=%0d result=%b, expected %b", av, nv, result, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(8'hA1, 0, 8'hA1);  // no rotation
    check(8'hA1, 3, 8'h0D);  // matches the constant-amount test
    check(8'hA1, 7, 8'hD0);
    check(8'hA1, 8, 8'hA1);  // amount == width -- reduces to 0
    check(8'hA1, 11, 8'h0D); // amount > width -- reduces to 3
    check(8'hFF, 4, 8'hFF);  // all-ones is invariant under rotation
    check(8'h01, 1, 8'h02);

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
