// Drives examples/call.tr (`Top`, whose rule calls the pure helper
// `Avg`) through real module ports. The second case (a=200, b=100)
// deliberately overflows bits[8] inside `Avg`'s own `let sum = a + b`:
// 300 mod 256 = 44, then >> 1 = 22 -- proving the inlined call reuses
// the same modular-add semantics as any other `+`, not some wider
// intermediate the call boundary might otherwise hide.
`timescale 1ns/1ps

module call_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .a(a),
    .b(b),
    .result(result)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'd20;
    b = 8'd30;

    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd25) begin
      $display("SIMULATION FAILED: result=%0d, expected 25 ((20+30)>>1)", result);
      $finish;
    end

    a = 8'd200;
    b = 8'd100;
    repeat (1) @(posedge clock);
    #1;
    if (result !== 8'd22) begin
      $display("SIMULATION FAILED: result=%0d, expected 22 ((200+100 mod 256)>>1 = 44>>1)", result);
      $finish;
    end

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
