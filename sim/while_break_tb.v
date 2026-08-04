// Drives examples/while_break.tr: proves `break` genuinely exits the
// loop early in real hardware, not just that firtool accepts the
// rendered mux text -- a mid-loop break (limit=5), an immediate break
// on the very first iteration (limit=0), and the loop never even being
// entered at all (go=0, the loop's OWN condition false from the start,
// unrelated to `break`).
`timescale 1ns/1ps

module while_break_tb;
  reg clock = 0;
  reg reset = 1;
  reg [7:0] limit = 0;
  reg go = 0;
  wire [7:0] result;
  integer fail = 0;

  WhileBreak dut (
    .clock(clock),
    .reset(reset),
    .limit(limit),
    .go(go),
    .result(result)
  );

  always #5 clock = ~clock;

  task check(input [7:0] exp, input [127:0] label);
    begin
      if (result !== exp) begin
        $display("SIMULATION FAILED: %0s: result=%0d (expected %0d)",
                  label, result, exp);
        fail = 1;
      end
    end
  endtask

  initial begin
    repeat (3) @(posedge clock);
    #1;
    reset = 0;

    // `break`'s own condition (`cnt >= limit`) reads cnt's OLD (pre-
    // edge) value, same as any register read -- so with limit=5, the
    // break lands one increment later than a naive read suggests: acc
    // reaches 6, not 5, by the time cnt's old value first satisfies
    // `>= 5` (at cnt=5, acc has already incremented a 6th time in the
    // SAME cycle, since acc := acc + 1 precedes the if in program
    // order and always executes on the breaking iteration too).
    limit = 8'd5;
    go = 1;
    repeat (20) @(posedge clock);
    #1;
    check(8'd6, "limit=5, go held high: breaks with acc settled at 6");

    limit = 8'd0;
    repeat (20) @(posedge clock);
    #1;
    check(8'd1, "limit=0: breaks after exactly one iteration");

    go = 0;
    limit = 8'd3;
    repeat (20) @(posedge clock);
    #1;
    check(8'd0, "go=0: while's own condition never true, zero iterations");

    if (fail) begin
      $display("SIMULATION FAILED");
    end else begin
      $display("SIMULATION PASSED");
    end
    $finish;
  end
endmodule
