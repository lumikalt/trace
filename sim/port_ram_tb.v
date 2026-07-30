// Drives examples/port_ram.tr through real module ports (`addr`,
// `write_data`, `write_en`, `read_data`) — no hierarchical peek/poke,
// no `--disable-opt` (a real output port keeps firtool from DCE-ing
// the design, same as sim/accumulator_tb.v).
//
// Proves memory is fully loadable and observable through ordinary
// ports: write distinct words to two addresses and read each back
// without aliasing (a `mem`, unlike a `reg`, has no defined reset
// value — there is no `= init` syntax for it — so this checks
// addressing correctness, not a reset default that doesn't exist); and
// confirms `write` outranks `read` on the same cycle (declared via
// `schedule { urgency write > read }`), so `read_data` holds its old
// value on a cycle where `write` fires instead of racing it.
`timescale 1ns/1ps

module port_ram_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] addr = 0;
  reg [15:0] write_data = 0;
  reg write_en = 0;
  wire [15:0] read_data;

  PortRam dut (
    .clock(clock),
    .reset(reset),
    .addr(addr),
    .write_data(write_data),
    .write_en(write_en),
    .read_data(read_data)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Write 0xABCD to address 5.
    addr = 8'd5;
    write_data = 16'hABCD;
    write_en = 1'b1;
    @(posedge clock);
    #1;

    // Same cycle write fires: read must NOT race it, so read_data
    // still holds its post-reset default.
    if (read_data !== 16'h0000) begin
      $display("FAIL: read_data should still be 0 while write fires, got %h", read_data);
      failed = 1;
    end

    // Stop writing; `read` now fires every cycle at address 5.
    write_en = 1'b0;
    repeat (2) @(posedge clock);
    #1;
    if (read_data !== 16'hABCD) begin
      $display("FAIL: expected read_data == 0xABCD at address 5, got %h", read_data);
      failed = 1;
    end

    // Write a different word to a different address, and confirm it
    // doesn't alias address 5.
    addr = 8'd9;
    write_data = 16'h1234;
    write_en = 1'b1;
    @(posedge clock);
    #1;
    write_en = 1'b0;
    repeat (2) @(posedge clock);
    #1;
    if (read_data !== 16'h1234) begin
      $display("FAIL: expected read_data == 0x1234 at address 9, got %h", read_data);
      failed = 1;
    end

    addr = 8'd5;
    repeat (2) @(posedge clock);
    #1;
    if (read_data !== 16'hABCD) begin
      $display("FAIL: address 5 should still hold 0xABCD, address 9's write must not alias it, got %h", read_data);
      failed = 1;
    end

    $display("final: read_data=%h", read_data);

    if (failed) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
