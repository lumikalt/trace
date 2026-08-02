// Icarus Verilog testbench for examples/struct_pair.tr — confirms a
// struct-typed reg (`p`) and struct-typed output (`result`) both
// compile to the expected flat `{name}_{field}` registers/ports, that
// a struct literal write (`p := Pair{...}`) updates every field
// together in one cycle, and that reading two fields off the SAME
// struct-typed local via `let {valid, data} = p` (sugar for one `let
// bind = p.field` per field, see DESIGN.md's "Locals" section) into a
// fresh struct literal round-trips both values correctly. `fill`
// (writes `p`) and `pass` (reads `p`) are scheduled mutually exclusive
// (a same-cycle read/write hazard on `p`), so this also exercises that
// ordinary conflict rule reaching a struct-typed reg the same way it
// would a plain one. `counter` (`bump`, `Pair{ data: counter.data + 1,
// ..counter }`) confirms struct update over real cycles: `counter_
// valid` self-connects and must stay 1 every cycle (never glitch to 0),
// while `counter_data` increments by exactly one per cycle -- proving
// `..counter` reads the PRE-write value each time, not some
// within-cycle feedback of the value `bump` is currently computing.
// `sample_valid` (`let {valid, ..} = p; last_valid := valid`) fires
// under the same guard as `pass` and confirms a NON-exhaustive
// destructuring pattern (`..` discarding `data`) reads the named field
// correctly rather than silently misconnecting when a field is skipped.
module struct_pair_tb;
    reg clock = 0;
    reg reset = 1;

    StructPair dut (
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

        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: input fifo should start empty after reset");
            failed = 1;
        end
        if (dut.p_valid !== 1'b0 || dut.p_data !== 8'h00) begin
            $display("FAIL: p should reset to {valid: 0, data: 0}, got valid=%b data=%h",
                      dut.p_valid, dut.p_data);
            failed = 1;
        end
        if (dut.result_valid !== 1'b0 || dut.result_data !== 8'h00) begin
            $display("FAIL: result should reset to {valid: 0, data: 0}, got valid=%b data=%h",
                      dut.result_valid, dut.result_data);
            failed = 1;
        end
        if (dut.last_valid !== 1'b0) begin
            $display("FAIL: last_valid should reset to 0, got %b", dut.last_valid);
            failed = 1;
        end
        // `counter` resets to {valid: 1, data: 0}, but `bump` also
        // fires on the very cycle `reset` deasserts (this testbench's
        // own blocking `reset = 0` lands in the same delta cycle as the
        // 3rd `repeat` edge, same as every other testbench's reset
        // release in this codebase) -- data reads 1 here, not 0.
        if (dut.counter_valid !== 1'b1 || dut.counter_data !== 8'h01) begin
            $display("FAIL: expected counter = {valid: 1, data: 1} right after reset releases, got valid=%b data=%h",
                      dut.counter_valid, dut.counter_data);
            failed = 1;
        end

        // Push a value into `input`; `fill` fires next cycle, writing
        // the whole struct `p` in one shot. `pass` must NOT fire the
        // same cycle (read/write hazard on `p`), so `result` stays 0.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: expected input to be drained after fill fired");
            failed = 1;
        end
        if (dut.p_valid !== 1'b1 || dut.p_data !== 8'h2A) begin
            $display("FAIL: expected p = {valid: 1, data: 0x2A}, got valid=%b data=%h",
                      dut.p_valid, dut.p_data);
            failed = 1;
        end
        if (dut.result_valid !== 1'b0 || dut.result_data !== 8'h00) begin
            $display("FAIL: expected result to still be {valid: 0, data: 0} (pass shouldn't fire alongside fill), got valid=%b data=%h",
                      dut.result_valid, dut.result_data);
            failed = 1;
        end
        if (dut.last_valid !== 1'b0) begin
            $display("FAIL: expected last_valid to still be 0 (sample_valid shouldn't fire alongside fill), got %b",
                      dut.last_valid);
            failed = 1;
        end

        // Next cycle: `input` is empty, so `fill` can't fire and `pass`
        // does, copying both of `p`'s fields into `result` together.
        @(posedge clock);
        #1;
        if (dut.result_valid !== 1'b1 || dut.result_data !== 8'h2A) begin
            $display("FAIL: expected result = {valid: 1, data: 0x2A}, got valid=%b data=%h",
                      dut.result_valid, dut.result_data);
            failed = 1;
        end
        if (dut.last_valid !== 1'b1) begin
            $display("FAIL: expected last_valid = 1 (sample_valid fires alongside pass), got %b",
                      dut.last_valid);
            failed = 1;
        end
        // Two more posedges have happened since (the `fill` cycle, then
        // the `pass` cycle) -- `bump` fires unconditionally every one
        // of them, so `counter_data` should read 1 + 2 = 3.
        if (dut.counter_valid !== 1'b1 || dut.counter_data !== 8'h03) begin
            $display("FAIL: expected counter = {valid: 1, data: 3}, got valid=%b data=%h",
                      dut.counter_valid, dut.counter_data);
            failed = 1;
        end

        // Three more cycles: counter_valid must stay 1 throughout (the
        // self-connect never glitches), counter_data keeps incrementing.
        repeat (3) @(posedge clock);
        #1;
        if (dut.counter_valid !== 1'b1 || dut.counter_data !== 8'h06) begin
            $display("FAIL: expected counter = {valid: 1, data: 6}, got valid=%b data=%h",
                      dut.counter_valid, dut.counter_data);
            failed = 1;
        end

        $display("final: p_valid=%b p_data=%h result_valid=%b result_data=%h last_valid=%b counter_valid=%b counter_data=%h",
                  dut.p_valid, dut.p_data, dut.result_valid, dut.result_data,
                  dut.last_valid, dut.counter_valid, dut.counter_data);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
