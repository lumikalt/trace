// Icarus Verilog testbench for examples/call_fifo.tr — the same
// forward-path/backpressure proof as sim/fifo_bridge_tb.v, but for
// `Bridge`'s logic wrapped in a callee (a `let`-bound Deq'd local
// threaded into an Enq, both fifo ops folded into `transfer`'s own
// guard through the call boundary) instead of written directly in the
// rule. Confirms the call-boundary folding produces the IDENTICAL
// handshaking, not just that firtool accepts the emitted text.
module call_fifo_tb;
    reg clock = 0;
    reg reset = 1;

    CallFifoBridge dut (
        .clock(clock),
        .reset(reset)
    );

    always #5 clock = ~clock;

    reg failed;

    initial begin
        failed = 0;

        reset = 1;
        repeat (3) @(posedge clock);
        reset = 0;
        #1;

        if (dut.__fifo_input_valid !== 1'b0 || dut.__fifo_output_valid !== 1'b0) begin
            $display("FAIL: fifos should start empty after reset");
            failed = 1;
        end

        // 1. Forward path: place a value in `input`, let `transfer` fire.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.__fifo_output_valid !== 1'b1 || dut.__fifo_output_data !== 8'h2A) begin
            $display("FAIL: expected output to hold 0x2A, got valid=%b data=%h",
                      dut.__fifo_output_valid, dut.__fifo_output_data);
            failed = 1;
        end
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: expected input to be drained after transfer fired");
            failed = 1;
        end
        if (dut.last !== 8'h2A) begin
            $display("FAIL: expected the callee's return value (last) to be 0x2A, got %h", dut.last);
            failed = 1;
        end

        // 2. Backpressure: output is still full; place a second value in
        // input and confirm `transfer` does NOT fire.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h55;
        @(posedge clock);
        #1;
        if (dut.__fifo_input_valid !== 1'b1 || dut.__fifo_input_data !== 8'h55) begin
            $display("FAIL: input should still hold 0x55 (transfer must stall while output is full)");
            failed = 1;
        end
        if (dut.__fifo_output_valid !== 1'b1 || dut.__fifo_output_data !== 8'h2A) begin
            $display("FAIL: output should be unchanged while stalled, got valid=%b data=%h",
                      dut.__fifo_output_valid, dut.__fifo_output_data);
            failed = 1;
        end

        // Drain `output` out-of-band and confirm the pending transfer
        // completes next cycle.
        dut.__fifo_output_valid = 1'b0;
        @(posedge clock);
        #1;
        if (dut.__fifo_output_valid !== 1'b1 || dut.__fifo_output_data !== 8'h55) begin
            $display("FAIL: expected the stalled 0x55 to transfer once output drained, got valid=%b data=%h",
                      dut.__fifo_output_valid, dut.__fifo_output_data);
            failed = 1;
        end
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: expected input to be drained after the stalled transfer fired");
            failed = 1;
        end
        if (dut.last !== 8'h55) begin
            $display("FAIL: expected last to now be 0x55, got %h", dut.last);
            failed = 1;
        end

        $display("final: input_valid=%b output_valid=%b output_data=%h last=%h",
                  dut.__fifo_input_valid, dut.__fifo_output_valid, dut.__fifo_output_data, dut.last);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
