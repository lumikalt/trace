// Drives examples/call_prio.tr (`Top`, whose rule calls `RoundRobin`,
// whose body is `return prio(reqs)`) through real module ports --
// proves the priority-mux chain `compile_prio` builds picks the
// LOWEST set bit, not just whichever encoding happens to fall out of
// how the mux nests, across several request patterns.
`timescale 1ns/1ps

module call_prio_tb;
  reg clock = 0;
  reg reset = 1;
  reg [3:0] reqs = 0;
  wire [1:0] grant;

  Top dut (
    .clock(clock),
    .reset(reset),
    .reqs(reqs),
    .grant(grant)
  );

  always #5 clock = ~clock;

  task check(input [3:0] r, input [1:0] expected);
    begin
      reqs = r;
      @(posedge clock);
      #1;
      if (grant !== expected) begin
        $display("SIMULATION FAILED: reqs=%b grant=%0d, expected %0d", r, grant, expected);
        $finish;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    check(4'b0001, 0); // only bit 0 set
    check(4'b0010, 1); // only bit 1 set
    check(4'b1100, 2); // bits 2,3 set -- lowest (2) wins
    check(4'b1111, 0); // all set -- lowest (0) wins
    check(4'b0000, 0); // none set -- defined fallback

    $display("final: grant=%0d", grant);
    $display("SIMULATION PASSED");
    $finish;
  end
endmodule
