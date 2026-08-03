// Icarus testbench for examples/logic_probe.tr, proving `logic <expr>`
// through real firtool + simulation: `input_ready`/`x_valid` are pure
// status reads (no side effect of their own), and coexist correctly
// with `drain`'s REAL dequeue of the SAME fifo in the SAME cycle
// (`schedule { conflict_free { probe, drain } }`) -- `input_ready`
// reflects the fifo's occupancy as of the START of the cycle even on a
// cycle where `drain` also fires and clears it, proving the probe
// doesn't itself disturb anything and isn't disturbed by a concurrent
// real touch either.
module logic_probe_tb;
    reg clock = 0;
    reg reset = 1;
    reg [7:0] x = 0;

    LogicProbe dut (
        .clock(clock),
        .reset(reset),
        .x(x),
        .input_ready(),
        .x_valid(),
        .consumed()
    );

    always #5 clock = ~clock;

    reg failed;

    initial begin
        failed = 0;

        reset = 1;
        repeat (3) @(posedge clock);
        reset = 0;
        #1;

        // 1. Empty fifo: input_ready must read 0, nothing to drain.
        if (dut.input_ready !== 1'b0) begin
            $display("FAIL(1): expected input_ready=0 with an empty fifo, got %b", dut.input_ready);
            failed = 1;
        end

        // 2. x=0 fails Validate's guard: x_valid must read 0.
        x = 8'h00;
        @(posedge clock);
        #1;
        if (dut.x_valid !== 1'b0) begin
            $display("FAIL(2): expected x_valid=0 for x=0, got %b", dut.x_valid);
            failed = 1;
        end

        // 3. x nonzero: x_valid reads 1, independent of the fifo entirely.
        x = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.x_valid !== 1'b1) begin
            $display("FAIL(3): expected x_valid=1 for x=0x2A, got %b", dut.x_valid);
            failed = 1;
        end

        // 4. Load the fifo, then check `input_ready` AND `consumed` both
        // update the SAME cycle -- `probe`'s read and `drain`'s real
        // dequeue coexist correctly. input_ready must read 1 (occupancy
        // as of the START of this cycle, before drain's own dequeue
        // takes effect), consumed must show the drained value, and the
        // fifo must actually be empty afterward.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h55;
        @(posedge clock);
        #1;
        if (dut.input_ready !== 1'b1) begin
            $display("FAIL(4): expected input_ready=1 the same cycle the fifo drains, got %b",
                      dut.input_ready);
            failed = 1;
        end
        if (dut.consumed !== 8'h55) begin
            $display("FAIL(4): expected consumed=0x55, got %h", dut.consumed);
            failed = 1;
        end
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL(4): expected the fifo to actually be drained (valid=0)");
            failed = 1;
        end

        // 5. Next cycle: fifo is empty again, input_ready drops back to 0
        // -- proving `probe` re-reads fresh occupancy every cycle, not a
        // stale latched value from step 4.
        @(posedge clock);
        #1;
        if (dut.input_ready !== 1'b0) begin
            $display("FAIL(5): expected input_ready=0 once the fifo is drained, got %b",
                      dut.input_ready);
            failed = 1;
        end

        $display("final: input_ready=%b x_valid=%b consumed=%h",
                  dut.input_ready, dut.x_valid, dut.consumed);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
