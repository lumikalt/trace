// Icarus Verilog testbench for examples/struct_pair.tr — confirms a
// struct-typed reg (`p`) and struct-typed output (`result`) both
// compile to the expected flat `{name}_{field}` registers/ports, that
// a struct literal write (`p := Pair{...}`) updates every field
// together in one cycle, and that reading two fields off the SAME
// struct-typed local (`p.valid`, `p.data`) into a fresh struct literal
// (`result := Pair{valid: p.valid, data: p.data}`) round-trips both
// values correctly. `fill` (writes `p`) and `pass` (reads `p`) are
// scheduled mutually exclusive (a same-cycle read/write hazard on
// `p`), so this also exercises that ordinary conflict rule reaching
// a struct-typed reg the same way it would a plain one.
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

        // Next cycle: `input` is empty, so `fill` can't fire and `pass`
        // does, copying both of `p`'s fields into `result` together.
        @(posedge clock);
        #1;
        if (dut.result_valid !== 1'b1 || dut.result_data !== 8'h2A) begin
            $display("FAIL: expected result = {valid: 1, data: 0x2A}, got valid=%b data=%h",
                      dut.result_valid, dut.result_data);
            failed = 1;
        end

        $display("final: p_valid=%b p_data=%h result_valid=%b result_data=%h",
                  dut.p_valid, dut.p_data, dut.result_valid, dut.result_data);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
