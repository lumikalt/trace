// Drives examples/checksum.tr through real firtool + Icarus.
//
// `m` has no port-based way to load it (this example is about
// reassigned locals, not port-based memory access -- see
// examples/port_ram.tr for that), so this testbench pokes
// `dut.m_ext.Memory[i]` directly, same hierarchical-path pattern as
// sim/subleq_tb.v. `m` is also never WRITTEN anywhere in the design,
// which firtool's default optimizer apparently treats as license to
// constant-fold `result` to 0 regardless of memory contents -- despite
// the real `result` output port, this STILL needs `--disable-opt`,
// confirmed empirically (without it, `result` reads back as a hardcoded
// 0 in the generated Verilog, not a bug in this design).
//
// mem[0..3] = 10, 20, 30, 40 -- expect result == 100. If `s`'s
// reassignments were compiled last-assignment-wins (the bug this
// feature fixes), each `s := s + m[i]` would see a STALE `s`, and the
// final sum would come out wrong.
`timescale 1ns/1ps

module checksum_tb;
  reg clock = 0;
  reg reset = 1;
  wire [15:0] result;

  Checksum dut (
    .clock(clock),
    .reset(reset),
    .result(result)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    dut.m_ext.Memory[0] = 16'd10;
    dut.m_ext.Memory[1] = 16'd20;
    dut.m_ext.Memory[2] = 16'd30;
    dut.m_ext.Memory[3] = 16'd40;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    @(posedge clock);
    #1;

    $display("final: result=%0d", result);

    if (result !== 16'd100) begin
      $display("FAIL: expected result == 100, got %0d", result);
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
