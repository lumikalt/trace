// Drives examples/dynamic_bit_select.tr through real module ports. x =
// 0xE3 (1110_0011) stays fixed while `i` changes between TWO different
// values across two cycles -- proving all three outputs are genuinely
// read at a runtime-computed position, not baked in at synthesis time.
// i=7 deliberately runs `up` (x[i +: 4]) past the top of x's own 8 bits
// (needing bits up to index 10), proving the "phantom" high bits a
// dshr-based dynamic shift zero-fills are handled correctly, not just
// the always-in-range case i=4 covers for every output.
`timescale 1ns/1ps

module dynamic_bit_select_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] x = 0;
  reg [2:0] i = 0;
  wire bit_out;
  wire [3:0] up, down;

  Top dut (
    .clock(clock),
    .reset(reset),
    .x(x),
    .i(i),
    .bit_out(bit_out),
    .up(up),
    .down(down)
  );

  always #5 clock = ~clock;

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;
    x = 8'hE3;
    i = 3'd4;

    repeat (1) @(posedge clock);
    #1;
    if (bit_out !== 1'b0) begin
      $display("SIMULATION FAILED: i=4 bit_out=%0b, expected 0 (bit 4 of 0xE3)", bit_out);
      $finish;
    end
    if (up !== 4'he) begin
      $display("SIMULATION FAILED: i=4 up=%0h, expected e (x[7:4])", up);
      $finish;
    end
    if (down !== 4'h1) begin
      $display("SIMULATION FAILED: i=4 down=%0h, expected 1 (x[4:1])", down);
      $finish;
    end

    i = 3'd7;
    repeat (1) @(posedge clock);
    #1;
    if (bit_out !== 1'b1) begin
      $display("SIMULATION FAILED: i=7 bit_out=%0b, expected 1 (bit 7 of 0xE3)", bit_out);
      $finish;
    end
    if (up !== 4'h1) begin
      $display("SIMULATION FAILED: i=7 up=%0h, expected 1 (only bit 7 survives, rest zero-padded)", up);
      $finish;
    end
    if (down !== 4'he) begin
      $display("SIMULATION FAILED: i=7 down=%0h, expected e (x[7:4])", down);
      $finish;
    end

    $display("final: i4 bit=0 up=e down=1; i7 bit=%0b up=%0h down=%0h", bit_out, up, down);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
