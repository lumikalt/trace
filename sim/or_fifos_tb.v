// Icarus testbench for examples/or_fifos.tr, proving `or` through real
// firtool + simulation: `pick_default`'s chain ends in a default (`0`),
// so it fires every cycle regardless of fifo occupancy (`tick_count`
// advances unconditionally); `pick_strict`'s chain has no default, so it
// stays fallible (`fired_strict` is 0, and nothing advances, whenever
// neither `c` nor `d` has data). Both chains pick in priority order
// (first alternative wins) and leave the loser fifo untouched.
module or_fifos_tb;
    reg clock = 0;
    reg reset = 1;

    OrFifos dut (
        .clock(clock),
        .reset(reset),
        .result_default(),
        .tick_count(),
        .result_strict(),
        .fired_strict()
    );

    always #5 clock = ~clock;

    reg failed;
    reg [7:0] tc0;

    initial begin
        failed = 0;
        reset = 1;
        repeat (3) @(posedge clock);
        reset = 0;
        #1;

        // 1. Neither `a` nor `b` ready: `pick_default` uses its default
        // (0), but still fires -- `tick_count` must already have
        // advanced past reset.
        if (dut.result_default !== 8'h00) begin
            $display("FAIL(1): expected default result_default=0, got %h", dut.result_default);
            failed = 1;
        end
        tc0 = dut.tick_count;
        @(posedge clock);
        #1;
        if (dut.tick_count !== tc0 + 8'h1) begin
            $display("FAIL(1): expected tick_count to advance unconditionally, got %d vs %d",
                      dut.tick_count, tc0);
            failed = 1;
        end

        // 2. Neither `c` nor `d` ready: `pick_strict` does NOT fire.
        if (dut.fired_strict !== 1'b0) begin
            $display("FAIL(2): expected fired_strict=0 when neither c nor d ready");
            failed = 1;
        end

        // 3. Load both `a` and `b`: `a` wins priority, `b` left untouched.
        dut.__fifo_a_valid = 1'b1;
        dut.__fifo_a_data = 8'h11;
        dut.__fifo_b_valid = 1'b1;
        dut.__fifo_b_data = 8'h22;
        @(posedge clock);
        #1;
        if (dut.result_default !== 8'h11) begin
            $display("FAIL(3): expected a (0x11) to win, got %h", dut.result_default);
            failed = 1;
        end
        if (dut.__fifo_a_valid !== 1'b0) begin
            $display("FAIL(3): expected `a` drained");
            failed = 1;
        end
        if (dut.__fifo_b_valid !== 1'b1 || dut.__fifo_b_data !== 8'h22) begin
            $display("FAIL(3): expected `b` untouched, got valid=%b data=%h",
                      dut.__fifo_b_valid, dut.__fifo_b_data);
            failed = 1;
        end

        // 4. Only `b` ready now: `b` wins.
        @(posedge clock);
        #1;
        if (dut.result_default !== 8'h22) begin
            $display("FAIL(4): expected b (0x22) to win, got %h", dut.result_default);
            failed = 1;
        end
        if (dut.__fifo_b_valid !== 1'b0) begin
            $display("FAIL(4): expected `b` drained");
            failed = 1;
        end

        // 5. Load only `d`: `pick_strict` fires (no `c`, so `d` wins by
        // priority fallthrough), `fired_strict` goes high.
        dut.__fifo_d_valid = 1'b1;
        dut.__fifo_d_data = 8'h44;
        @(posedge clock);
        #1;
        if (dut.result_strict !== 8'h44 || dut.fired_strict !== 1'b1) begin
            $display("FAIL(5): expected d (0x44) to win with fired_strict=1, got result=%h fired=%b",
                      dut.result_strict, dut.fired_strict);
            failed = 1;
        end
        if (dut.__fifo_d_valid !== 1'b0) begin
            $display("FAIL(5): expected `d` drained");
            failed = 1;
        end

        // 6. Neither `c` nor `d` ready again: `pick_strict` stalls, and
        // (like any other output register this emitter produces) simply
        // HOLDS its last-written value rather than resetting -- same
        // convention a register/output already follows everywhere else
        // in this language when its rule doesn't fire.
        @(posedge clock);
        #1;
        if (dut.fired_strict !== 1'b1 || dut.result_strict !== 8'h44) begin
            $display("FAIL(6): expected fired_strict/result_strict to HOLD (1/0x44) \
                       while stalled, got fired=%b result=%h", dut.fired_strict, dut.result_strict);
            failed = 1;
        end

        $display("final: result_default=%h tick_count=%d result_strict=%h fired_strict=%b",
                  dut.result_default, dut.tick_count, dut.result_strict, dut.fired_strict);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
