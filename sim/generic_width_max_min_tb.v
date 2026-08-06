// Drives examples/generic_width_max_min.tr (`Top`, whose rule calls two
// generic callees whose OWN return type is a `max`/`min` of their two
// implicit width params, `n`/`m`) through real module ports -- proves
// `max`/`min` resolve to the correct concrete width at the concrete call
// site (`n=4`, `m=8` here, from `a`/`b`'s own port widths), both as the
// callee's declared return type AND as `zext`/`trunc`'s own width
// argument reached through the `hint` mechanism (see `compile_zext`'s
// own doc comment on why that channel matters for a generic width
// expression like `max(n, m)`).
`timescale 1ns/1ps

module generic_width_max_min_tb;
  reg clock = 0;
  reg reset = 1;
  reg [3:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] wide;
  wire [3:0] narrow;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .wide(wide),
    .narrow(narrow)
  );

  always #5 clock = ~clock;

  task check(input [3:0] av, input [7:0] bv, input [7:0] wide_expected, input [3:0] narrow_expected);
    begin
      a = av;
      b = bv;
      @(posedge clock);
      #1;
      if (wide !== wide_expected || narrow !== narrow_expected) begin
        $display("SIMULATION FAILED: a=%h b=%h wide=%h (expected %h) narrow=%h (expected %h)",
          av, bv, wide, wide_expected, narrow, narrow_expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(4'hF, 8'h00, 8'h0F, 4'hF); // Wider zero-extends a into wide's 8 bits
    check(4'h3, 8'hFF, 8'h03, 4'h3); // narrow tracks a regardless of b

    $display("final: wide=%0d narrow=%0d", wide, narrow);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
