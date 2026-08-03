// Drives examples/tb_accumulator_failing.tr's `Top`, whose checkpoint
// (`dut.sum = 99`) can never hold — proves a failing checkpoint actually
// stalls `__cont_drive` and stays observable from outside, rather than
// just being a claim. See sim/tb_accumulator_tb.v for the passing case.
`timescale 1ns/1ps

module tb_accumulator_failing_tb;
  reg clock = 0;
  reg reset = 1;

  Top dut (
    .clock(clock),
    .reset(reset)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    repeat (7) @(posedge clock);
    #1;

    $display("final: __cont_drive=%0d", dut.__cont_drive);
    if (dut.__cont_drive !== 3'd0) begin
      $display("SIMULATION FAILED: stalled at segment %0d (checkpoint failed)", dut.__cont_drive);
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
