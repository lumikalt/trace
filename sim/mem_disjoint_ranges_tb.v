// Drives examples/mem_disjoint_ranges.tr through real module ports.
//
// Proves schedule.rs's FIFTH disjointness argument end to end: `i`'s
// proven range is [0,5), `j`'s is [5,10) -- they never overlap, so
// `write` (writes `m[i]`) and `read` (reads `m[j]`) both fire
// unconditionally every cycle (confirmed via the generated FIRRTL:
// `fires_write`/`fires_read` are both a bare `UInt<1>(1)`, and
// tests/firrtl.rs's own dedicated test confirms there is no `assert(`
// at all for the mem pair specifically), even though `i` and `j` share
// no base, multiplier, or offset relationship whatsoever.
//
// Unlike sim/mem_disjoint_bounded_tb.v (same base, needs cycle-accurate
// address tracking to check `y` against the right prior write), this
// design's two addresses never touch the same cell at all: `read`'s
// address always stays in [5,10), a range `write` never reaches, so `y`
// must show the untouched reset contents on every single cycle, not
// just at specific tracked points.
`timescale 1ns/1ps

module mem_disjoint_ranges_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] y;
  wire [7:0] write_count;
  wire [7:0] read_count;

  MemDisjointRanges dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .y(y),
    .write_count(write_count),
    .read_count(read_count)
  );

  always #5 clock = ~clock;

  reg failed;
  integer cycle;
  integer addr;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Addresses 5..9 are never written by this design at all -- `write`
    // always addresses [0,5) (`i`'s own proven range). Preload a
    // sentinel there to prove that claim, not just assume the mem's own
    // reset value is 0. Addresses 0..4 get a DIFFERENT sentinel, so the
    // check after the drive loop below can confirm `write` actually
    // landed there at least once -- not just that `read`'s half of the
    // disjointness claim held.
    for (addr = 0; addr < 5; addr = addr + 1) begin
      dut.m_ext.Memory[addr] = 8'h55;
    end
    for (addr = 5; addr < 10; addr = addr + 1) begin
      dut.m_ext.Memory[addr] = 8'hAA;
    end

    // Drive x for more than one full period of both `i` and `j` (each
    // period 5) to exercise every address on both sides at least once.
    for (cycle = 0; cycle < 12; cycle = cycle + 1) begin
      x = 8'h10 + cycle;
      @(posedge clock);
      #1;

      // `read`'s address (`j`) always stays in [5,10), a range `write`
      // never touches -- `y` must show the untouched sentinel every
      // cycle once the read pipeline has primed (skip the very first
      // post-reset cycle, whose read reflects whatever preceded it).
      if (cycle > 0 && y !== 8'hAA) begin
        $display(
          "FAIL: cycle %0d: expected y == 0xAA (address never written), got %h",
          cycle,
          y
        );
        failed = 1;
      end
    end

    // Neither rule ever stalled across all 12 cycles above -- under the
    // old, fully conservative model (no proof at all for two different,
    // unrelated bases), `read` would never have gotten a turn.
    if (write_count !== 8'd12) begin
      $display("FAIL: expected write_count == 12 (write never stalls), got %0d", write_count);
      failed = 1;
    end
    if (read_count !== 8'd12) begin
      $display(
        "FAIL: expected read_count == 12 -- read must fire every cycle just like write, since \
         their accesses are proven disjoint; got %0d",
        read_count
      );
      failed = 1;
    end

    // Every address `read` could ever reach (5..9) still holds the
    // untouched sentinel -- confirmed once more here directly against
    // the mem's own contents, not just `y`.
    for (addr = 5; addr < 10; addr = addr + 1) begin
      if (dut.m_ext.Memory[addr] !== 8'hAA) begin
        $display(
          "FAIL: expected Memory[%0d] == 0xAA (never written), got %h",
          addr,
          dut.m_ext.Memory[addr]
        );
        failed = 1;
      end
    end

    // Every address `write` could ever reach (0..4) must have moved OFF
    // its own preloaded sentinel -- 12 cycles is more than one full
    // period of `i` (5), so every address in [0,5) gets written at
    // least once. This is the other half of the disjointness claim: not
    // just that `read`'s side stayed untouched, but that `write`'s side
    // genuinely fired there.
    for (addr = 0; addr < 5; addr = addr + 1) begin
      if (dut.m_ext.Memory[addr] === 8'h55) begin
        $display(
          "FAIL: expected Memory[%0d] to have been written at least once, still 0x55",
          addr
        );
        failed = 1;
      end
    end

    $display("final: write_count=%0d read_count=%0d y=%h", write_count, read_count, y);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
