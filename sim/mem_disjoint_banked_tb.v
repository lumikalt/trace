// Drives examples/mem_disjoint_banked.tr through real module ports.
//
// Proves schedule.rs's banking disjointness proof (a shared power-of-
// two multiplier, TWO GENUINELY DIFFERENT bases `i`/`j`) end to end --
// there is no `conflict_free` annotation anywhere in this design, and
// `write`/`read` both fire unconditionally every cycle (confirmed via
// the generated FIRRTL: `fires_write`/`fires_read` are both a bare
// `UInt<1>(1)`).
//
// Unlike mem_disjoint_affine_tb.v (the same-base case, `i`/`i+1`), the
// point here is specifically that `i` and `j` are UNRELATED inputs --
// the three cycles below drive several combinations, INCLUDING `i == j`,
// proving the bank argument holds even when the two registers' values
// happen to coincide, not just when they provably differ.
`timescale 1ns/1ps

module mem_disjoint_banked_tb;
  reg clock = 0;
  reg reset = 1;
  reg [3:0] i = 0;
  reg [3:0] j = 0;
  reg [7:0] x = 0;
  wire [7:0] y;
  wire [7:0] write_count;
  wire [7:0] read_count;

  MemDisjointBanked dut (
    .clock(clock),
    .reset(reset),
    .i(i),
    .j(j),
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

    // Preload the odd (read-bank) addresses `read` will observe below,
    // bypassing the design's own write logic entirely -- same trick
    // mem_disjoint_rw_tb.v/mem_disjoint_affine_tb.v use for their own
    // mem contents.
    dut.m_ext.Memory[1] = 8'hAA;
    dut.m_ext.Memory[3] = 8'hBB;
    dut.m_ext.Memory[11] = 8'hCC;

    // Cycle 1: i=0 (write even address 0), j=0 (read odd address 1).
    i = 4'd0;
    j = 4'd0;
    x = 8'h11;
    @(posedge clock);
    #1;
    if (y !== 8'hAA) begin
      $display("FAIL: cycle 1 expected y == 0xAA (address 1), got %h", y);
      failed = 1;
    end

    // Cycle 2: i=0 again (write even address 0, new data), j=1 (read
    // odd address 3).
    i = 4'd0;
    j = 4'd1;
    x = 8'h22;
    @(posedge clock);
    #1;
    if (y !== 8'hBB) begin
      $display("FAIL: cycle 2 expected y == 0xBB (address 3), got %h", y);
      failed = 1;
    end

    // Cycle 3: i == j == 5 -- write even address 10, read odd address
    // 11, from the SAME input VALUE. The proof only depends on the
    // multiplier's parity, not on i and j actually differing, so this
    // must be just as safe as the cycles above.
    i = 4'd5;
    j = 4'd5;
    x = 8'h33;
    @(posedge clock);
    #1;
    if (y !== 8'hCC) begin
      $display("FAIL: cycle 3 (i == j == 5) expected y == 0xCC (address 11), got %h", y);
      failed = 1;
    end

    // Neither rule ever stalled: both fired every one of the 3 cycles
    // above, exactly like mem_disjoint_rw_tb.v's write_count/read_count
    // check -- under the old, fully conservative model (no proof at
    // all), `read` would never have gotten a turn.
    if (write_count !== 8'd3) begin
      $display("FAIL: expected write_count == 3 (write never stalls), got %0d", write_count);
      failed = 1;
    end
    if (read_count !== 8'd3) begin
      $display(
        "FAIL: expected read_count == 3 -- read must fire every cycle just like write, since \
         their accesses are proven disjoint; got %0d",
        read_count
      );
      failed = 1;
    end

    // Functional correctness of the writes themselves: address 0 holds
    // cycle 2's data (0x22, overwriting cycle 1's 0x11), address 10
    // holds cycle 3's data (0x33).
    if (dut.m_ext.Memory[0] !== 8'h22) begin
      $display("FAIL: expected Memory[0] == 0x22, got %h", dut.m_ext.Memory[0]);
      failed = 1;
    end
    if (dut.m_ext.Memory[10] !== 8'h33) begin
      $display("FAIL: expected Memory[10] == 0x33, got %h", dut.m_ext.Memory[10]);
      failed = 1;
    end

    $display(
      "final: write_count=%0d read_count=%0d y=%h Memory[0]=%h Memory[10]=%h",
      write_count,
      read_count,
      y,
      dut.m_ext.Memory[0],
      dut.m_ext.Memory[10]
    );

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
