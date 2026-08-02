// Icarus Verilog testbench for examples/call_struct_param.tr — confirms
// a struct-typed fn param (`UsePair`) and an `?T`-typed fn param
// (`Consume`) both resolve a REG-typed argument correctly (not just a
// literal one): `pair`/`opt` are written by `fill_pair`/`fill_opt`,
// read back through the CALLEE's own param, and `Consume`'s bare-
// statement guard (`o?`) folds through the param binding into
// `read_opt`'s own rule guard, so `from_opt` only updates on a cycle
// `opt` is actually present.
module call_struct_param_tb;
    reg clock = 0;
    reg reset = 1;
    reg go = 0;
    wire [7:0] from_pair, from_opt;

    CallStructParam dut (
        .clock(clock),
        .reset(reset),
        .go(go),
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

        // Push a value into `input`; `fill_pair` fires next cycle,
        // writing the whole struct `pair`. `read_pair` must NOT fire
        // the same cycle (read/write hazard on `pair`).
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h2A;
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

        // Next cycle: `input` is empty, so `fill_pair` can't fire and
        // `read_pair` does, calling `UsePair(pair)` -- the struct-typed
        // PARAM must resolve through to `pair`'s own flat registers.
        @(posedge clock);
        #1;
        if (from_pair !== 8'h2A) begin
            $display("FAIL: expected from_pair = 0x2A via UsePair(pair), got %h", from_pair);
            failed = 1;
        end

        // Same story for `opt`/`Consume`: `go` triggers `fill_opt`,
        // `read_opt` fires (and updates `from_opt`) only the cycle
        // after, once `opt` is present and `fill_opt` isn't firing.
        go = 1;
        @(posedge clock);
        #1;
        go = 0;
        if (dut.opt_valid !== 1'b1 || dut.opt_data !== 8'd42) begin
            $display("FAIL: expected opt = present(42), got valid=%b data=%d",
                      dut.opt_valid, dut.opt_data);
            failed = 1;
        end
        if (from_opt !== 8'h00) begin
            $display("FAIL: expected from_opt still 0 (read_opt shouldn't fire alongside fill_opt), got %h", from_opt);
            failed = 1;
        end

        @(posedge clock);
        #1;
        if (from_opt !== 8'd42) begin
            $display("FAIL: expected from_opt = 42 via Consume(opt), got %d", from_opt);
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
