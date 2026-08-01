// Drives examples/reassigned_local.tr through real module ports.
//
// Proves a rule-local reassigned within one rule resolves each
// reference at its OWN textual position, through real hardware: drives
// several distinct (a, b) pairs across consecutive cycles and checks
// `first_val == a` (x's value as of its FIRST binding) and `second_val == b`
// (x's value as of its LATER, reassigned binding) every single cycle
// -- never aliased to each other, never lagging a cycle behind. A
// naive last-assignment-wins compile (the old lazy design, if the
// preflight rejection had simply been deleted with no position
// tracking) would make `first_val` ALSO read `b`.
`timescale 1ns/1ps

module reassigned_local_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] first_val;
  wire [7:0] second_val;

  ReassignedLocal dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .first_val(first_val),
    .second_val(second_val)
  );

  always #5 clock = ~clock;

  reg failed;
  integer i;
  reg [7:0] avals [0:3];
  reg [7:0] bvals [0:3];

  initial begin
    failed = 0;
    avals[0] = 8'd10; bvals[0] = 8'd200;
    avals[1] = 8'd1;  bvals[1] = 8'd2;
    avals[2] = 8'd99; bvals[2] = 8'd0;
    avals[3] = 8'd255; bvals[3] = 8'd128;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    for (i = 0; i < 4; i = i + 1) begin
      a = avals[i];
      b = bvals[i];
      @(posedge clock);
      #1;
      if (first_val !== avals[i]) begin
        $display("FAIL: cycle %0d expected first_val == %0d, got %0d", i, avals[i], first_val);
        failed = 1;
      end
      if (second_val !== bvals[i]) begin
        $display("FAIL: cycle %0d expected second_val == %0d, got %0d", i, bvals[i], second_val);
        failed = 1;
      end
    end

    $display("final: first_val=%0d second_val=%0d", first_val, second_val);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
