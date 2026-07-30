// Icarus Verilog testbench for examples/fifo_bridge.tr, driving the
// firtool-generated Verilog directly (`trace examples/fifo_bridge.tr
// --firrtl | firtool --disable-opt`).
//
// FifoBridge has no module ports (fifos are internal-only in this
// language — there is no fifo-port concept, only scalar `input`/
// `output`), so this testbench reaches into the design with
// hierarchical paths, exactly like sim/subleq_tb.v: `dut.__fifo_
// input_valid`, `dut.__fifo_input_data`, etc. Those are firtool's
// straight pass-through of the register names firrtl.rs emits (see
// src/firrtl.rs's `fifo_valid_name`/`fifo_data_name`), so they hold
// across recompiles as long as those helpers don't change.
//
// This proves the actual claim from DESIGN.md's opening example: the
// compiler derives ready/valid handshaking from the two failure
// conditions (`Deq[]` fails when empty, `Enq[x]` fails when full) with
// no stall logic in the source. Two things get checked:
//   1. the forward path: a value placed in `input` reaches `output`
//      unchanged, one cycle later;
//   2. backpressure: `transfer` must NOT fire (must NOT drop or
//      overwrite `input`) while `output` is still full, and must fire
//      again as soon as `output` is drained.
module fifo_bridge_tb;
    reg clock = 0;
    reg reset = 1;

    FifoBridge dut (
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

        // Both fifos start empty after reset.
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

        // 2. Backpressure: output is still full; place a second value in
        // input and confirm `transfer` does NOT fire (input must not be
        // silently dropped or overwritten, output must not change).
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

        // Drain `output` out-of-band (standing in for an external
        // consumer, since there is no fifo port to Deq through) and
        // confirm the pending transfer completes next cycle.
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

        $display("final: input_valid=%b output_valid=%b output_data=%h",
                  dut.__fifo_input_valid, dut.__fifo_output_valid, dut.__fifo_output_data);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
