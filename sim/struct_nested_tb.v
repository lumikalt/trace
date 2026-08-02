// Icarus Verilog testbench for examples/struct_nested.tr — confirms a
// NESTED struct (`Frame.header : Header`) flattens all the way down to
// per-leaf-field registers/ports (`f_header_valid`, `f_header_seq`,
// `f_data`), that a nested struct literal write (`f := Frame{header:
// Header{...}, data: ...}`) updates every leaf field together in one
// cycle, and that a chained field read (`f.header.valid`) resolves
// correctly through both levels into a freshly-built nested literal
// (`result := Frame{header: Header{...}, data: ...}`).
module struct_nested_tb;
    reg clock = 0;
    reg reset = 1;

    StructNested dut (
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
        if (dut.f_header_valid !== 1'b0 || dut.f_header_seq !== 4'h0 || dut.f_data !== 8'h00) begin
            $display("FAIL: f should reset to {header: {valid: 0, seq: 0}, data: 0}, got valid=%b seq=%h data=%h",
                      dut.f_header_valid, dut.f_header_seq, dut.f_data);
            failed = 1;
        end
        if (dut.result_header_valid !== 1'b0 || dut.result_header_seq !== 4'h0 || dut.result_data !== 8'h00) begin
            $display("FAIL: result should reset to all zero, got valid=%b seq=%h data=%h",
                      dut.result_header_valid, dut.result_header_seq, dut.result_data);
            failed = 1;
        end

        // Push a value into `input`; `fill` fires next cycle, writing
        // the WHOLE nested struct `f` (both levels) in one shot, and
        // advancing `seq`. `pass` must NOT fire the same cycle
        // (read/write hazard on `f`), so `result` stays 0.
        dut.__fifo_input_valid = 1'b1;
        dut.__fifo_input_data = 8'h2A;
        @(posedge clock);
        #1;
        if (dut.__fifo_input_valid !== 1'b0) begin
            $display("FAIL: expected input to be drained after fill fired");
            failed = 1;
        end
        if (dut.f_header_valid !== 1'b1 || dut.f_header_seq !== 4'h0 || dut.f_data !== 8'h2A) begin
            $display("FAIL: expected f = {header: {valid: 1, seq: 0}, data: 0x2A}, got valid=%b seq=%h data=%h",
                      dut.f_header_valid, dut.f_header_seq, dut.f_data);
            failed = 1;
        end
        if (dut.seq !== 4'h1) begin
            $display("FAIL: expected seq to advance to 1, got %h", dut.seq);
            failed = 1;
        end
        if (dut.result_header_valid !== 1'b0) begin
            $display("FAIL: expected result to still be reset (pass shouldn't fire alongside fill), got valid=%b",
                      dut.result_header_valid);
            failed = 1;
        end

        // Next cycle: `input` is empty, so `fill` can't fire and `pass`
        // does, copying both levels of `f`'s fields into `result`.
        @(posedge clock);
        #1;
        if (dut.result_header_valid !== 1'b1 || dut.result_header_seq !== 4'h0 || dut.result_data !== 8'h2A) begin
            $display("FAIL: expected result = {header: {valid: 1, seq: 0}, data: 0x2A}, got valid=%b seq=%h data=%h",
                      dut.result_header_valid, dut.result_header_seq, dut.result_data);
            failed = 1;
        end

        $display("final: f_header_valid=%b f_header_seq=%h f_data=%h result_header_valid=%b result_header_seq=%h result_data=%h",
                  dut.f_header_valid, dut.f_header_seq, dut.f_data,
                  dut.result_header_valid, dut.result_header_seq, dut.result_data);

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
