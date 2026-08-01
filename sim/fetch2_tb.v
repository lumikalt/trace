// Drives examples/fetch2.tr through real firtool + Icarus, proving the
// spawn/sync lowering produces correct parallel hardware, not just
// plausible-looking FIRRTL text.
//
// pc = 3 at the trigger cycle; bank0[3] = 0xAAAA, bank1[4] = 0xBBBB are
// the values the two spawned FSMs must read. `pc` is then changed to 7
// one cycle after the trigger, with bank0[7]/bank1[8] holding DIFFERENT
// (wrong) values -- if either ReadBank's own segments read `pc` directly
// instead of a trigger-time save register, the still-in-flight read
// would pick up the changed address and `ir` would come out wrong. This
// was confirmed to actually discriminate: a hand-lowered variant with
// the `__arg_*` save registers removed (segments reading `pc`/`pc + 1`
// directly) was run against this exact scenario and produced
// ir=1111xxxx instead of aaaabbbb before this file was trusted as a
// regression test.
`timescale 1ns/1ps

module fetch2_tb;
  reg clock = 0;
  reg reset = 1;
  reg [15:0] pc = 3;
  wire [31:0] ir;

  Fetch2 dut (
    .clock(clock),
    .reset(reset),
    .pc(pc),
    .ir(ir)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    dut.bank0_ext.Memory[3] = 16'hAAAA;
    dut.bank1_ext.Memory[4] = 16'hBBBB;
    dut.bank0_ext.Memory[7] = 16'h1111;
    dut.bank1_ext.Memory[8] = 16'h2222;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Trigger cycle: fetch2_s0 fires, saves pc/pc+1 into the two spawns'
    // own arg registers.
    @(posedge clock);
    #1;
    // Change pc right after -- a correct lowering must not observe this
    // for the fetch already in flight.
    pc = 7;

    repeat (4) @(posedge clock);
    #1;

    $display("final: ir=%0h", ir);

    if (ir !== 32'hAAAABBBB) begin
      $display("FAIL: expected ir == AAAABBBB (trigger-time pc==3), got %0h", ir);
      failed = 1;
    end

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
