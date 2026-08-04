// Drives examples/mem_disjoint_affine.tr through real module ports.
//
// Proves schedule.rs's affine-offset disjointness proof (the same-base
// `i`/`i+1` case, v2 of mem_disjoint_rw.tr's constants-only v1) end to
// end -- there is no `conflict_free` annotation anywhere in this
// design at all, and `write`/`read` both fire unconditionally every
// cycle (confirmed via `--explain-schedule` and the generated FIRRTL:
// `fires_write`/`fires_read` are both a bare `UInt<1>(1)`, no stall
// between them).
//
// Two cases, both same-cycle as their own write:
//   1. i=5: write lands at address 5, read (address 6) is UNTOUCHED by
//      it and reflects a value preloaded there directly.
//   2. i=15 (the wraparound boundary): write lands at address 15, read
//      (address 15+1 truncated to 4 bits = 0) is UNTOUCHED and reflects
//      a value preloaded at address 0 -- this is the specific case that
//      exercises the proof's OWN soundness argument (modular wraparound
//      exactly matching the mem's power-of-two depth), not just "two
//      arbitrary addresses happen to differ."
`timescale 1ns/1ps

module mem_disjoint_affine_tb;
  reg clock = 0;
  reg reset = 1;
  reg [3:0] i = 0;
  reg [7:0] x = 0;
  wire [7:0] y;

  MemDisjointAffine dut (
    .clock(clock),
    .reset(reset),
    .i(i),
    .x(x),
    .y(y)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Case 1: mid-range, no wraparound.
    dut.m_ext.Memory[6] = 8'hAA;
    i = 4'd5;
    x = 8'h11;
    @(posedge clock);
    #1;

    if (dut.m_ext.Memory[5] !== 8'h11) begin
      $display("FAIL: expected Memory[5] == 0x11 (write's own address), got %h", dut.m_ext.Memory[5]);
      failed = 1;
    end
    if (dut.m_ext.Memory[6] !== 8'hAA) begin
      $display(
        "FAIL: expected Memory[6] == 0xAA (untouched by the concurrent write to address 5), got %h",
        dut.m_ext.Memory[6]
      );
      failed = 1;
    end
    if (y !== 8'hAA) begin
      $display(
        "FAIL: expected y == 0xAA (read of address 6 must not have stalled behind the write to \
         address 5), got %h",
        y
      );
      failed = 1;
    end

    // Case 2: the wraparound boundary, i=15 -> i+1 truncates to 0. This
    // is the case that actually exercises the proof's modular-
    // arithmetic argument, not just "two arbitrary addresses differ".
    dut.m_ext.Memory[0] = 8'hBB;
    i = 4'd15;
    x = 8'h22;
    @(posedge clock);
    #1;

    if (dut.m_ext.Memory[15] !== 8'h22) begin
      $display(
        "FAIL: expected Memory[15] == 0x22 (write's own address), got %h",
        dut.m_ext.Memory[15]
      );
      failed = 1;
    end
    if (dut.m_ext.Memory[0] !== 8'hBB) begin
      $display(
        "FAIL: expected Memory[0] == 0xBB (untouched -- i+1 wraps 15 -> 0, must still be a \
         DIFFERENT address from 15, not alias it), got %h",
        dut.m_ext.Memory[0]
      );
      failed = 1;
    end
    if (y !== 8'hBB) begin
      $display(
        "FAIL: expected y == 0xBB (read of the wrapped address 0 must not have stalled behind \
         the write to address 15), got %h",
        y
      );
      failed = 1;
    end

    $display(
      "final: Memory[5]=%h Memory[6]=%h Memory[15]=%h Memory[0]=%h y=%h",
      dut.m_ext.Memory[5],
      dut.m_ext.Memory[6],
      dut.m_ext.Memory[15],
      dut.m_ext.Memory[0],
      y
    );

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
