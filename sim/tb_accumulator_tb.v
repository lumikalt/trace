// Drives examples/tb_accumulator.tr's `Top` — itself a testbench written
// in trace, instantiating `Accumulator` and driving it through real
// clock cycles via a `<sequences>` rule, ending in a checkpoint
// (`dut.sum = 26`, an ordinary bare comparison). `Top` has no ports at
// all — the checkpoint's pass/fail lives entirely in whether its
// `<sequences>` continuation register (`__cont_drive`) wraps back to 0
// or sticks at the segment that failed, so this harness reaches in
// hierarchically (`dut.__cont_drive`) the same way sim/fifo_bridge_tb.v
// does for FifoBridge, another port-less DUT.
`timescale 1ns/1ps

module tb_accumulator_tb;
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

    // 7 states (see the .tr file's own `drive` rule): 6 driving cycles
    // plus one more to check `dut.sum` against the expected value.
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
