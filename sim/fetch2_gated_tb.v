// Companion to sim/fetch2_tb.v: the spawn trigger sits behind `go`,
// held low for several cycles after reset before being pulsed. Proves
// the spawn lowering's plain-0 continuation-register reset is safe
// even when a callee's own segment-0 guard (`__cont_h1 == 0`) is
// trivially true well before any real trigger -- the spurious early
// completion this could cause is never observable: fetch2's own
// trigger rule always renders (and so wins derived-stall priority)
// before the callee's segment rules, the trigger resets `done` to 0
// whenever it fires, and `sync` can't observe `done` until the
// trigger's own segment has run at least once. See DESIGN.md's "sync,
// race, spawn" section for the full argument.
`timescale 1ns/1ps

module fetch2_gated_tb;
  reg clock = 0;
  reg reset = 1;
  reg [15:0] pc = 3;
  reg go = 0;
  wire [31:0] ir;

  Fetch2Gated dut (
    .clock(clock),
    .reset(reset),
    .pc(pc),
    .go(go),
    .ir(ir)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    dut.bank0_ext.Memory[3] = 16'hAAAA;
    dut.bank1_ext.Memory[4] = 16'hBBBB;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Hold `go` low for several cycles: with a naive 0-reset, this is
    // exactly the window a callee's own segment-0 rule could fire
    // spuriously before ever being triggered.
    repeat (5) @(posedge clock);
    #1;
    go = 1;
    @(posedge clock);
    #1;
    go = 0;

    repeat (5) @(posedge clock);
    #1;

    $display("final: ir=%0h", ir);

    if (ir !== 32'hAAAABBBB) begin
      $display("FAIL: expected ir == AAAABBBB, got %0h", ir);
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
