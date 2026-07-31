// Drives examples/conflict_free_mem.tr through real module ports.
//
// Proves `conflict_free { write, read }` genuinely lets `read` fire on
// every cycle, including cycles where `write` ALSO fires -- unlike
// port_ram_tb.v, which drives its own `urgency`-stalled memory and
// explicitly checks read_data does NOT update on a write cycle. Here
// read_data must track a newly-changed read_addr on the exact same
// cycle a write to a DIFFERENT address is also happening, rather than
// needing an extra non-writing cycle to "catch up" the way a
// derived stall would force.
`timescale 1ns/1ps

module conflict_free_mem_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] write_addr = 0;
  reg [15:0] write_data = 0;
  reg write_en = 0;
  reg [7:0] read_addr = 0;
  wire [15:0] read_data;

  ConflictFreeMem dut (
    .clock(clock),
    .reset(reset),
    .write_addr(write_addr),
    .write_data(write_data),
    .write_en(write_en),
    .read_addr(read_addr),
    .read_data(read_data)
  );

  always #5 clock = ~clock;

  reg failed;

  initial begin
    failed = 0;

    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // Settle address 5 to 0x1111.
    write_addr = 8'd5;
    write_data = 16'h1111;
    write_en = 1'b1;
    @(posedge clock);
    #1;
    write_en = 1'b0;
    @(posedge clock);
    #1;

    // Settle address 9 to 0x2222.
    write_addr = 8'd9;
    write_data = 16'h2222;
    write_en = 1'b1;
    @(posedge clock);
    #1;
    write_en = 1'b0;
    @(posedge clock);
    #1;

    // Confirm address 5 reads back correctly first.
    read_addr = 8'd5;
    @(posedge clock);
    #1;
    if (read_data !== 16'h1111) begin
      $display("FAIL: expected read_data == 0x1111 at address 5, got %h", read_data);
      failed = 1;
    end

    // The real proof: on the SAME cycle, switch read_addr to 9 AND
    // fire a write to a THIRD address (2) -- if `read` had stalled the
    // way port_ram.tr's forced urgency stall would, read_data would
    // still show address 5's stale value here, needing an extra
    // non-writing cycle to catch up. It must not.
    read_addr = 8'd9;
    write_addr = 8'd2;
    write_data = 16'h3333;
    write_en = 1'b1;
    @(posedge clock);
    #1;
    write_en = 1'b0;
    if (read_data !== 16'h2222) begin
      $display(
        "FAIL: read did not track the new read_addr on the same cycle as a concurrent write \
         (got %h, expected 0x2222 -- read must not have stalled)",
        read_data
      );
      failed = 1;
    end

    // Confirm the concurrent write to address 2 actually landed too --
    // not just that read didn't stall, but that the write itself was
    // correct.
    read_addr = 8'd2;
    @(posedge clock);
    #1;
    if (read_data !== 16'h3333) begin
      $display("FAIL: expected read_data == 0x3333 at address 2, got %h", read_data);
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
