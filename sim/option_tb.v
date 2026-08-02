// Icarus Verilog testbench for examples/option.tr — confirms an
// `?T`-typed reg (`opt`) resets to absent (`false`, i.e. `opt_valid ==
// 0`), that a plain-value write coerces implicitly to present (both
// `opt_valid`/`opt_data` update together), that the `?` unwrap
// (`result := opt?`) only takes effect on a cycle where `opt` is
// present (its guard folds into `pass`'s own rule guard, same
// mechanism a fifo `Deq[]` already uses), and that the non-failing
// `.valid`/`.data` if/else escape hatch (`check`) tracks presence
// independently of whether `pass` actually fires. `fill` (writes
// `opt`) is scheduled mutually exclusive with `pass`/`check` (both
// read `opt`) — the same struct-typed read/write hazard rule
// `struct_pair_tb.v` already exercises, generalized to `?T`. `relayed`
// is an `?T`-typed OUTPUT port, written from `check` -- exercises the
// output-specific write-threading bookkeeping (`__out_{port}_{field}`
// internal registers, `struct_reg_source` keyed by the port name)
// directly, not just inferred from the reg case above.
module option_tb;
    reg clock = 0;
    reg reset = 1;

    Option dut (
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
        if (dut.opt_valid !== 1'b0) begin
            $display("FAIL: opt should reset to absent (false), got valid=%b data=%h",
                      dut.opt_valid, dut.opt_data);
            failed = 1;
        end

        // `check` has no guard, so it fires every cycle `fill` doesn't
        // -- with `input` still empty, that's every cycle so far, and
        // it should already have observed `opt` absent.
        @(posedge clock);
        #1;
        if (dut.was_present !== 1'b0) begin
            $display("FAIL: expected was_present=0 while opt is absent, got %b", dut.was_present);
            failed = 1;
        end
        if (dut.result !== 8'h00) begin
            $display("FAIL: expected result=0 while opt is absent (pass's guard must not fire), got %h", dut.result);
            failed = 1;
        end
        if (dut.relayed_valid !== 1'b0) begin
            $display("FAIL: expected relayed absent while opt is absent, got valid=%b data=%h",
                      dut.relayed_valid, dut.relayed_data);
            failed = 1;
        end

        // Push a value into `input`; `fill` fires next cycle, coercing
        // it into `opt` as the present case. `pass`/`check` must NOT
        // fire the same cycle (read/write hazard on `opt`), so
        // `result`/`was_present` stay unchanged.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: expected input to be drained after fill fired");
            failed = 1;
        end
        if (dut.opt_valid !== 1'b1 || dut.opt_data !== 8'h2A) begin
            $display("FAIL: expected opt = present(0x2A), got valid=%b data=%h",
                      dut.opt_valid, dut.opt_data);
            failed = 1;
        end
        if (dut.result !== 8'h00 || dut.was_present !== 1'b0) begin
            $display("FAIL: expected result/was_present unchanged the same cycle as fill, got result=%h was_present=%b",
                      dut.result, dut.was_present);
            failed = 1;
        end

        // Next cycle: `input` is empty, so `fill` can't fire and both
        // `pass` (unwrap via `?`) and `check` (`.valid`/`.data`) do.
        @(posedge clock);
        #1;
        if (dut.result !== 8'h2A) begin
            $display("FAIL: expected result = 0x2A after unwrap, got %h", dut.result);
            failed = 1;
        end
        if (dut.was_present !== 1'b1) begin
            $display("FAIL: expected was_present=1 after check observes opt present, got %b", dut.was_present);
            failed = 1;
        end
        if (dut.relayed_valid !== 1'b1 || dut.relayed_data !== 8'h2A) begin
            $display("FAIL: expected relayed = present(0x2A), got valid=%b data=%h",
                      dut.relayed_valid, dut.relayed_data);
            failed = 1;
        end

        $display("final: opt_valid=%b opt_data=%h result=%h was_present=%b relayed_valid=%b relayed_data=%h",
                  dut.opt_valid, dut.opt_data, dut.result, dut.was_present, dut.relayed_valid, dut.relayed_data);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
