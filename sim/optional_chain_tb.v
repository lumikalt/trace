`timescale 1ns/1ps
module tb;
  reg clock = 0;
  reg reset = 1;
  reg do_write = 0;
  reg [1:0] mode = 0;
  reg [7:0] c_in = 0;
  wire [3:0] count;
  wire [7:0] result;
  OptionalChain dut(.clock(clock), .reset(reset), .do_write(do_write), .mode(mode), .c_in(c_in), .count(count), .result(result));

  always #5 clock = ~clock;

  task tick;
    begin
      @(posedge clock);
      #1;
    end
  endtask

  task pulse_write(input [1:0] m, input [7:0] cv);
    begin
      mode = m;
      c_in = cv;
      do_write = 1;
      tick;
      do_write = 0;
    end
  endtask

  reg [3:0] settled;
  initial begin
    tick; tick;
    reset = 0;
    tick; tick;

    // `a` present, `b` absent: `nav` must NOT fire -- the case that
    // discriminates a genuine multi-hop fold from one that only checks
    // the final hop's own presence bit.
    pulse_write(1, 0);
    tick; tick; tick;
    if (count !== 0) begin
      $display("FAIL: nav fired while intermediate hop (b) was absent: count=%0d", count);
      $finish;
    end

    // `a` present, `b` present, c=42: `nav` fires every cycle, reading 42.
    pulse_write(2, 42);
    tick; tick; tick;
    if (count == 0) begin
      $display("FAIL: nav never fired once the whole chain was present");
      $finish;
    end
    if (result !== 42) begin
      $display("FAIL: expected result=42, got result=%0d", result);
      $finish;
    end

    // `a` fully absent again: `nav` must stop firing.
    pulse_write(3, 0);
    tick; tick;
    settled = count;
    tick; tick; tick;
    if (count !== settled) begin
      $display("FAIL: nav kept firing after `a` went fully absent: settled=%0d, now=%0d", settled, count);
      $finish;
    end

    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
