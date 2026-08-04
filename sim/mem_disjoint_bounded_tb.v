// Drives examples/mem_disjoint_bounded.tr through real module ports.
//
// Proves bounds.rs's statically-proven register-bound feature end to
// end: `m`'s depth (10) is deliberately NOT a power of two, so this
// design's write/read disjointness could NOT have been proven by v2 or
// v3 (schedule.rs's other two proofs, both gated on a power-of-two
// depth) -- only the proven bound on `i` (`where i < 9`) makes it
// provable at all, via schedule.rs's own real-integer disjointness
// argument. `write`/`read` both fire unconditionally every cycle
// (confirmed via the generated FIRRTL: `fires_write`/`fires_read` are
// both a bare `UInt<1>(1)`, and tests/firrtl.rs's own dedicated test
// confirms there is no `assert(` at all for the mem pair specifically).
//
// `i` is an internal register (not a port), entirely self-driven by
// `bump`'s own logic (0 -> 1 -> ... -> 8 -> 0, repeating) -- peeked
// hierarchically (`dut.i`) only to know which address each cycle's
// write/read touch, since the correctness check below needs to predict
// that sequence.
`timescale 1ns/1ps

module mem_disjoint_bounded_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  wire [7:0] y;
  wire [7:0] write_count;
  wire [7:0] read_count;

  MemDisjointBounded dut (
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
  reg [7:0] prev_x;
  reg [3:0] i_before;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Address 0 is never written by this design: the write address is
    // ALWAYS `i + 1`, and `i`'s own proven bound (`< 9`) caps it at 8,
    // so `i + 1` never reaches 0 again once the design starts running.
    // Preload a sentinel to prove that claim, not just assume it.
    dut.m_ext.Memory[0] = 8'hAA;

    // Drive one full period of `i` (0 through 8, 9 cycles) plus one
    // extra cycle past the wraparound back to 0, checking every cycle
    // along the way.
    for (cycle = 0; cycle < 10; cycle = cycle + 1) begin
      // `i`'s PRE-edge value is what determines THIS cycle's write/read
      // addresses (`i + 1` / `i`) -- captured before the clock edge,
      // not after (`dut.i` right after the edge already reflects the
      // NEXT cycle's value, one step ahead of what this cycle used).
      i_before = dut.i;
      x = 8'h10 + cycle;
      @(posedge clock);
      #1;

      // Every cycle after the very first one: `read`'s address this
      // cycle is EXACTLY the address `write` landed at last cycle
      // (`i` this cycle == `i + 1` from last cycle, since `i`
      // increments by exactly 1 whenever it doesn't wrap) -- so `y`
      // must show last cycle's `x`, except right at the wraparound
      // boundary (`i`'s pre-edge value is 0 THIS cycle, meaning last
      // cycle reset it from 8 back to 0 -- this cycle reads address 0,
      // a cell this design never writes at all, so `y` must still show
      // the untouched sentinel).
      if (cycle > 0) begin
        if (i_before == 0) begin
          if (y !== 8'hAA) begin
            $display(
              "FAIL: cycle %0d (wraparound to i=0): expected y == 0xAA (address 0 untouched), \
               got %h",
              cycle,
              y
            );
            failed = 1;
          end
        end else if (y !== prev_x) begin
          $display(
            "FAIL: cycle %0d (i=%0d): expected y == %h (last cycle's write), got %h",
            cycle,
            i_before,
            prev_x,
            y
          );
          failed = 1;
        end
      end
      prev_x = x;
    end

    // Neither rule ever stalled across all 10 cycles above -- under the
    // old, fully conservative model (no proof at all for a non-power-
    // of-two depth), `read` would never have gotten a turn.
    if (write_count !== 8'd10) begin
      $display("FAIL: expected write_count == 10 (write never stalls), got %0d", write_count);
      failed = 1;
    end
    if (read_count !== 8'd10) begin
      $display(
        "FAIL: expected read_count == 10 -- read must fire every cycle just like write, since \
         their accesses are proven disjoint; got %0d",
        read_count
      );
      failed = 1;
    end

    // Address 0's sentinel survived every cycle, including the
    // wraparound one already checked above -- confirmed once more here
    // directly against the mem's own contents, not just `y`.
    if (dut.m_ext.Memory[0] !== 8'hAA) begin
      $display("FAIL: expected Memory[0] == 0xAA (never written), got %h", dut.m_ext.Memory[0]);
      failed = 1;
    end

    $display(
      "final: write_count=%0d read_count=%0d y=%h Memory[0]=%h",
      write_count,
      read_count,
      y,
      dut.m_ext.Memory[0]
    );

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
