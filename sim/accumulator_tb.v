// Drives examples/accumulator.tr through real module ports (`inc`,
// `sum`) — no hierarchical peek/poke. Unlike sim/subleq_tb.v this needs
// no `--disable-opt` either: an observable output port is enough for
// firtool to keep the logic. `-DSYNTHESIS` is still required (see
// sim/README.md) — that one's unrelated to ports, it just skips
// firtool's debug register-randomization block.
`timescale 1ns/1ps

module accumulator_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] inc = 0;
  wire [7:0] sum;

  Accumulator dut (
    .clock(clock),
    .reset(reset),
    .inc(inc),
    .sum(sum)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    inc = 8'd5;

    repeat (4) @(posedge clock);
    #1;
    if (sum !== 8'd20) begin
      $display("SIMULATION FAILED: sum=%0d, expected 20 (4 * 5)", sum);
      $finish;
    end

    inc = 8'd3;
    repeat (2) @(posedge clock);
    #1;
    if (sum !== 8'd26) begin
      $display("SIMULATION FAILED: sum=%0d, expected 26 (20 + 2 * 3)", sum);
      $finish;
    end

    $display("final: sum=%0d", sum);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
