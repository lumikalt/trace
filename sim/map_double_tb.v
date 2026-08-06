// Drives examples/map_double.tr -- proves `AdderTree(xs.map(Double(_)))`
// (a `map` over an elaboration-time list, unrolled by elaborate.rs's
// pre-pass into `((Double(a) + Double(b)) + (Double(c) + Double(d)))`,
// itself further reduced by `AdderTree`'s own recursion) computes the
// right sum through real hardware, including wrapping past `bits[32]` --
// not just that it compiles and firtool accepts it.
`timescale 1ns/1ps

module map_double_tb;
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
    if (total !== 32'd200) begin
      $display("SIMULATION FAILED: total=%0d, expected 200 (2*(10+20+30+40))", total);
      $finish;
    end

    // Wraps past bits[32] -- proves `map`'s per-element `Double` call
    // isn't silently widened anywhere along the way.
    a = 32'hFFFFFFFF;
    b = 32'd0;
    c = 32'd0;
    d = 32'd0;

    repeat (1) @(posedge clock);
    #1;
    if (total !== 32'hFFFFFFFE) begin
      $display("SIMULATION FAILED: total=%0h, expected fffffffe (wraps past bits[32])", total);
      $finish;
    end

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
