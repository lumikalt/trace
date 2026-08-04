// Drives examples/conflict_free_mem.tr through real module ports, the
// same DUT as conflict_free_mem_tb.v, but deliberately violates the
// precondition that testbench never exercises: `conflict_free { write,
// read }` claims safety on `write` and `read` firing together ONLY when
// their addresses genuinely differ (see conflict_free_mem.tr's own
// comment). Proves the compiler-inserted address-disjointness assertion
// is a real runtime check, not dead code that always passes: a safe
// window (write and read to different addresses, same cycle) produces
// no assertion failure, while a window that deliberately collides
// `write_addr` and `read_addr` on a cycle where both fire DOES produce
// one -- checked by grepping the simulator's own output for the exact
// message text `firrtl.rs` embeds in the `assert`.
`timescale 1ns/1ps

module conflict_free_mem_collision_tb;
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

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // The claim genuinely holds here: write and read fire together, but
    // to DIFFERENT addresses (5 and 9).
    write_addr = 8'd5;
    write_data = 16'h1111;
    write_en = 1'b1;
    read_addr = 8'd9;
    @(posedge clock);
    #1;
    write_en = 1'b0;
    repeat (2) @(posedge clock);
    #1;
    $display("SAFE WINDOW DONE: read_data=%h", read_data);

    // Deliberately violate the claim: write and read fire the SAME
    // cycle to the SAME address.
    write_addr = 8'd5;
    write_data = 16'h2222;
    write_en = 1'b1;
    read_addr = 8'd5;
    @(posedge clock);
    #1;
    write_en = 1'b0;

    // No "SIMULATION PASSED" here, deliberately -- same reasoning as
    // mutually_exclusive_check_tb.v: this testbench's whole point is to
    // TRIGGER the assertion, so tests/sim.rs checks for the assertion's
    // absence before the marker above and its presence after instead.
    $finish;
  end
endmodule
