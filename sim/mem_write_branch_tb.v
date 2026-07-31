// Drives examples/mem_write_branch.tr through real module ports.
//
// Proves a memory write nested in `if` (no `else`) is genuinely
// conditional -- `en` itself toggles, not just addr/data defaulting to
// 0 -- while an unconditional statement in the SAME rule (`count`'s
// increment) keeps running on every cycle regardless, something a
// top-level guard can't express (a guard gates the whole rule).
`timescale 1ns/1ps

module mem_write_branch_tb;
  reg clock = 0;
  reg reset = 1;
  reg we = 0;
  reg [3:0] addr = 0;
  reg [7:0] data = 0;
  reg [3:0] read_addr = 0;
  wire [7:0] read_data;
  wire [7:0] count;

  MemWriteBranch dut (
    .clock(clock),
    .reset(reset),
    .we(we),
    .addr(addr),
    .data(data),
    .read_addr(read_addr),
    .read_data(read_data),
    .count(count)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // `count` increments every cycle even while `we` stays 0 (no mem
    // write happening) -- proving the conditional write does not gate
    // the rest of the rule.
    repeat (3) @(posedge clock);
    #1;
    if (count !== 8'd3) begin
      $display("FAIL: count should be 3 after 3 non-writing cycles, got %0d", count);
      failed = 1;
    end

    // Write 0xAA to address 5 for one cycle, while count keeps
    // incrementing right alongside it.
    we = 1;
    addr = 4'd5;
    data = 8'hAA;
    @(posedge clock);
    #1;
    we = 0;

    repeat (2) @(posedge clock);
    #1;
    if (count !== 8'd6) begin
      $display("FAIL: count should be 6 after 6 total cycles (writing doesn't skip the increment), got %0d", count);
      failed = 1;
    end

    read_addr = 4'd5;
    repeat (2) @(posedge clock);
    #1;
    if (read_data !== 8'hAA) begin
      $display("FAIL: expected read_data == 0xAA at address 5, got %h", read_data);
      failed = 1;
    end

    // A later non-writing cycle at the same address must NOT clobber
    // the previously-written word -- proving `en` is genuinely false
    // (not just addr/data quietly defaulting to 0) when the `if` branch
    // isn't taken.
    data = 8'h55;
    repeat (2) @(posedge clock);
    #1;
    if (read_data !== 8'hAA) begin
      $display("FAIL: address 5 should still hold 0xAA -- a non-writing cycle must not overwrite it, got %h", read_data);
      failed = 1;
    end

    $display("final: count=%0d read_data=%h", count, read_data);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
