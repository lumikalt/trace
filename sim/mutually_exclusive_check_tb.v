// Drives examples/mutually_exclusive_check.tr through real module ports.
//
// Proves the compiler-inserted `mutually_exclusive` assertion is a
// genuine runtime check, not dead code that always passes: a cycle
// where the claim actually holds (`we_a`/`we_b` never both 1) produces
// no assertion failure, while a cycle that deliberately violates the
// claim (`we_a` and `we_b` both 1 the same cycle) DOES produce one --
// checked by grepping the simulator's own output for the exact message
// text `firrtl.rs` embeds in the `assert`.
`timescale 1ns/1ps

module mutually_exclusive_check_tb;
  reg clock = 0;
  reg reset = 1;
  reg we_a = 0;
  reg we_b = 0;
  wire [7:0] a, b;

  MutuallyExclusiveCheck dut (
    .clock(clock),
    .reset(reset),
    .we_a(we_a),
    .we_b(we_b),
    .a(a),
    .b(b)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // The claim genuinely holds here: never both 1 the same cycle.
    we_a = 1;
    we_b = 0;
    @(posedge clock);
    #1;
    we_a = 0;
    we_b = 1;
    @(posedge clock);
    #1;
    we_b = 0;
    repeat (2) @(posedge clock);
    #1;
    $display("SAFE WINDOW DONE: a=%h b=%h", a, b);

    // Deliberately violate the claim: both fire the same cycle.
    we_a = 1;
    we_b = 1;
    @(posedge clock);
    #1;
    we_a = 0;
    we_b = 0;

    // No "SIMULATION PASSED" here, deliberately: unlike every other
    // testbench in sim/, this one's whole point is to TRIGGER the
    // assertion in its second half, so a fixed pass/fail verdict
    // wouldn't mean anything -- tests/sim.rs checks for the assertion's
    // absence before the marker above and its presence after instead.
    $finish;
  end
endmodule
