// Two independent instances of examples/extmodule_tribuf.tr's `Top`
// sharing one physical bus (their `bus` ports tied together) — the real
// point of the whole `io`/`attach`/`extmodule` feature set: a genuine
// bidirectional net that either side can drive, with the other side
// sensing whatever's on it, and NEITHER driving is a compile-time-fixed
// direction (see DESIGN.md's "Module ports" section). tribuf.v (compiled
// alongside this testbench, see devenv.nix's `simulate` script) supplies
// TriBuf's real tri-state implementation; trace's own output only ever
// declares its interface (`extmodule ... : ... defname = TriBuf`).
`timescale 1ns/1ps

module extmodule_tribuf_tb;
  reg clock = 0;
  reg reset = 1;
  reg a_enable = 0, b_enable = 0;
  reg [7:0] a_data = 0, b_data = 0;
  wire [7:0] a_sensed, b_sensed;
  wire [7:0] bus;

  Top a (
    .clock(clock), .reset(reset),
    .enable(a_enable), .data(a_data), .sensed(a_sensed), .bus(bus)
  );
  Top b (
    .clock(clock), .reset(reset),
    .enable(b_enable), .data(b_data), .sensed(b_sensed), .bus(bus)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    a_enable = 1;
    a_data = 8'hAA;
    repeat (2) @(posedge clock);
    #1;
    if (b_sensed !== 8'hAA) begin
      $display("SIMULATION FAILED: b_sensed=%h, expected aa (a driving)", b_sensed);
      $finish;
    end

    a_enable = 0;
    b_enable = 1;
    b_data = 8'h55;
    repeat (2) @(posedge clock);
    #1;
    if (a_sensed !== 8'h55) begin
      $display("SIMULATION FAILED: a_sensed=%h, expected 55 (b driving)", a_sensed);
      $finish;
    end

    $display("final: b_sensed=%h a_sensed=%h", b_sensed, a_sensed);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
