// Icarus Verilog testbench for examples/call_struct_return.tr —
// confirms a struct-RETURNING fn (`MakePair`) and an `?T`-RETURNING
// `<fails>` fn (`Wrap`) both decompose correctly: `pair`/`opt` are
// written from the CALLEE's own return value (a fresh struct literal,
// and a present-coerced scalar respectively), and `Wrap`'s bare-
// statement guard (`(x <> 0)?`) folds through its return value into
// `fill_opt`'s own rule guard, so a zero item is never dequeued at all
// (the whole rule, fifo dequeue included, stays blocked) and `opt`
// holds its prior value instead of updating to a bogus "present(0)".
module call_struct_return_tb;
    reg clock = 0;
    reg reset = 1;
    wire [7:0] from_pair, from_opt;

    CallStructReturn dut (
        .clock(clock),
        .reset(reset),
        .from_pair(from_pair),
        .from_opt(from_opt)
    );

    always #5 clock = ~clock;

    reg failed;

    initial begin
        failed = 0;

        reset = 1;
        repeat (3) @(posedge clock);
        reset = 0;
        #1;

        if (dut.pair_valid !== 1'b0 || dut.opt_valid !== 1'b0) begin
            $display("FAIL: pair/opt should reset absent");
            failed = 1;
        end

        // Push a value into `pair_input`; `fill_pair` fires next cycle,
        // calling `MakePair(d)` and writing the WHOLE returned struct.
        // `read_pair` must NOT fire the same cycle (read/write hazard).
        dut.__fifo_pair_input_valid = 1'b1;
        dut.__fifo_pair_input_data = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.pair_valid !== 1'b1 || dut.pair_data !== 8'h2A) begin
            $display("FAIL: expected pair = {valid: 1, data: 0x2A}, got valid=%b data=%h",
                      dut.pair_valid, dut.pair_data);
            failed = 1;
        end
        if (from_pair !== 8'h00) begin
            $display("FAIL: expected from_pair still 0 (read_pair shouldn't fire alongside fill_pair), got %h", from_pair);
            failed = 1;
        end
        dut.__fifo_pair_input_valid = 1'b0;

        @(posedge clock);
        #1;
        if (from_pair !== 8'h2A) begin
            $display("FAIL: expected from_pair = 0x2A via MakePair's returned struct, got %h", from_pair);
            failed = 1;
        end

        // Push a NONZERO value into `opt_input`; `Wrap`'s guard `(x <>
        // 0)?` passes, so `fill_opt` fires, calling `Wrap(d)` and
        // writing the coerced-present `?T` return.
        dut.__fifo_opt_input_valid = 1'b1;
        dut.__fifo_opt_input_data = 8'd42;
        @(posedge clock);
        #1;
        if (dut.opt_valid !== 1'b1 || dut.opt_data !== 8'd42) begin
            $display("FAIL: expected opt = present(42), got valid=%b data=%d",
                      dut.opt_valid, dut.opt_data);
            failed = 1;
        end
        dut.__fifo_opt_input_valid = 1'b0;

        @(posedge clock);
        #1;
        if (from_opt !== 8'd42) begin
            $display("FAIL: expected from_opt = 42 via Wrap's returned ?T, got %d", from_opt);
            failed = 1;
        end

        // Push a ZERO value into `opt_input`; `Wrap`'s guard fails, so
        // `fill_opt` (fifo dequeue included) never fires at all — `opt`
        // must hold its prior value (42), not get overwritten with a
        // bogus "present(0)", and the item must stay stuck in the fifo.
        dut.__fifo_opt_input_valid = 1'b1;
        dut.__fifo_opt_input_data = 8'd0;
        @(posedge clock);
        #1;
        if (dut.opt_valid !== 1'b1 || dut.opt_data !== 8'd42) begin
            $display("FAIL: expected opt to hold at present(42) when Wrap's guard fails, got valid=%b data=%d",
                      dut.opt_valid, dut.opt_data);
            failed = 1;
        end
        if (dut.__fifo_opt_input_valid !== 1'b1) begin
            $display("FAIL: expected the zero item to stay stuck in opt_input (guard blocks the whole rule)");
            failed = 1;
        end

        $display("final: from_pair=%h from_opt=%d", from_pair, from_opt);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
