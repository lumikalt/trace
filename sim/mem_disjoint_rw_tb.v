// Drives examples/mem_disjoint_rw.tr through real module ports.
//
// Proves the compiler's OWN auto-derived disjointness proof (there is
// no `conflict_free` annotation anywhere in this design) actually
// removes the derived stall, not just that `--explain-schedule` says
// so. `write` and `read` are BOTH unconditional (fire every cycle) --
// the maximal-contention case: under the old, fully conservative
// model, `read` (lower urgency) would never get a turn at all, since
// `write` never blocks or goes idle for it to catch -- `read_count`
// would stay stuck at 0 forever while `write_count` climbed every
// cycle. Also confirms the two literal addresses (3 and 7) land
// independently correct values, not just that neither stalls.
`timescale 1ns/1ps

module mem_disjoint_rw_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] y;
  wire [7:0] write_count;
  wire [7:0] read_count;

  MemDisjointRw dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .y(y),
    .write_count(write_count),
    .read_count(read_count)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Preload address 7 (never written by the design itself) directly,
    // bypassing the DUT's own write logic entirely -- the same
    // hierarchical-poke pattern subleq_tb.v uses to load program memory.
    dut.m_ext.Memory[7] = 8'hAA;

    // Drive a fixed write value and let a handful of cycles pass. If
    // `read` had stalled the way the old conservative model would
    // force, read_count would never move past 0 no matter how many
    // cycles run.
    x = 8'h42;
    repeat (5) @(posedge clock);
    #1;

    if (write_count !== 8'd5) begin
      $display("FAIL: expected write_count == 5 (write never stalls), got %0d", write_count);
      failed = 1;
    end
    if (read_count !== 8'd5) begin
      $display(
        "FAIL: expected read_count == 5 -- read must fire every cycle just like write, since \
         their accesses are proven disjoint; got %0d (0 would mean read never got a turn, the \
         old conservative-model failure this feature fixes)",
        read_count
      );
      failed = 1;
    end

    // Functional correctness, not just "didn't stall": the write really
    // landed at address 3 (not 7), and `y` really tracks address 7's
    // preloaded value (not address 3's).
    if (dut.m_ext.Memory[3] !== 8'h42) begin
      $display(
        "FAIL: expected Memory[3] == 0x42 (write's own address), got %h",
        dut.m_ext.Memory[3]
      );
      failed = 1;
    end
    if (y !== 8'hAA) begin
      $display("FAIL: expected y == 0xAA (read's own address, preloaded), got %h", y);
      failed = 1;
    end

    $display(
      "final: write_count=%0d read_count=%0d y=%h Memory[3]=%h",
      write_count,
      read_count,
      y,
      dut.m_ext.Memory[3]
    );

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
