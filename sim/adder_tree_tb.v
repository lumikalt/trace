// Drives examples/adder_tree.tr -- proves `AdderTree([a, b, c, d])`
// (elaboration-time tree recursion over a 4-element list, unrolled by
// elaborate.rs's pre-pass into `((a + b) + (c + d))`) computes the right
// sum through real hardware, including wrapping past `bits[32]` -- not
// just that it compiles and firtool accepts it.
`timescale 1ns/1ps

module adder_tree_tb;
  reg clock = 0;
  reg reset = 1;
  reg [31:0] a = 0;
  reg [31:0] b = 0;
  reg [31:0] c = 0;
  reg [31:0] d = 0;
  wire [31:0] total;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .c(c),
    .d(d),
    .total(total)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 32'd10;
    b = 32'd20;
    c = 32'd30;
    d = 32'd40;

    repeat (1) @(posedge clock);
    #1;
    if (total !== 32'd100) begin
      $display("SIMULATION FAILED: total=%0d, expected 100 (10+20+30+40)", total);
      $finish;
    end

    // Wraps past bits[32] -- proves the tree's modular add isn't
    // silently widened anywhere along the way.
    a = 32'hFFFFFFFF;
    b = 32'd2;
    c = 32'd0;
    d = 32'd0;

    repeat (1) @(posedge clock);
    #1;
    if (total !== 32'd1) begin
      $display("SIMULATION FAILED: total=%0d, expected 1 (wraps past bits[32])", total);
      $finish;
    end

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
