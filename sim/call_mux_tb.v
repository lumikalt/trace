// Drives examples/call_mux.tr (`Top`, whose rule computes `mux(sel, a,
// b)`) through real module ports -- proves the FIRRTL `mux` primop
// `compile_mux` emits directly really does pick `a` when `sel` is 1 and
// `b` when `sel` is 0, with `a`/`b` held to distinguishable values so a
// mixed-up selector would be caught, not accidentally passed.
`timescale 1ns/1ps

module call_mux_tb;
  reg clock = 0;
  reg reset = 1;
  reg sel = 0;
  reg [7:0] a = 0;
  reg [7:0] b = 0;
  wire [7:0] result;

  Top dut (
    .clock(clock),
    .reset(reset),
    .sel(sel),
    .a(a),
    .b(b),
    .result(result)
  );

  always #5 clock = ~clock;

  task check(input s, input [7:0] expected);
    begin
      sel = s;
      @(posedge clock);
      #1;
      if (result !== expected) begin
        $display("SIMULATION FAILED: sel=%b result=%0h, expected %0h", s, result, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    a = 8'hAA;
    b = 8'h55;

    check(1'b1, 8'hAA); // sel=1 selects a
    check(1'b0, 8'h55); // sel=0 selects b

    $display("final: result=%0d", result);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
