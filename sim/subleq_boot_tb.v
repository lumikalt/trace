// Icarus Verilog testbench for examples/subleq_boot.tr, proving the
// boot-load design: unlike sim/subleq_tb.v (which pokes
// `dut.m_ext.Memory[i]` directly before reset even clears), this
// loads the SAME SUBLEQ program through the real load_addr/load_data/
// load_en ports while `booted == 0`, pulses `boot_done`, and only
// THEN expects `step`/`refill` to start running -- proving the
// `booted` interlock actually holds the CPU off during loading, not
// just that loading-then-running happens to work when done in the
// right order by luck.
//
// Deliberately splits the load into two halves with an idle gap in
// the MIDDLE (mimicking a real UART/SPI loader's dead time between
// bytes) rather than loading straight through: `load`'s own urgency
// over `step` already blocks `step` for free while `load_en` is
// actually asserted every cycle, so a straight-through load never
// actually exercises the `booted` guard itself -- only a gap with
// memory still HALF-loaded does. Confirmed by deliberately removing
// `step`'s `(booted == 1)?` guard and re-running this exact testbench:
// it reports SIMULATION FAILED via the mid-gap __cont_step check below
// (the FSM starts executing a half-loaded program during the gap) --
// proving this gap is what makes the test discriminate. Note the FINAL
// pc/mem checks near the bottom do NOT discriminate this specific
// breakage on their own: the broken variant still settles at
// pc=6/mem[11]=5 (step stalls under load's own urgency once it catches
// up to the not-yet-loaded second half, so only the timing is wrong,
// not this program's eventual result) -- the mid-gap check is the one
// load-bearing assertion here; don't remove it as "redundant" without
// re-verifying against the broken variant again.
//
// `pc`/`ir`/`mem` still have no output ports (only the loader side
// gained ports), so final-state observation stays hierarchical
// (`dut.pc`, `dut.__cont_step`, `dut.m_ext.Memory[i]`) -- same as
// subleq_tb.v. Program is the identical one subleq_tb.v uses; see
// that file for the full walkthrough of what each instruction does
// and why.
`timescale 1ns/1ps

module subleq_boot_tb;
    reg clock = 0;
    reg reset = 1;
    reg [15:0] load_addr = 0;
    reg [15:0] load_data = 0;
    reg load_en = 0;
    reg boot_done = 0;

    SubleqBoot dut (
        .clock(clock),
        .reset(reset),
        .load_addr(load_addr),
        .load_data(load_data),
        .load_en(load_en),
        .boot_done(boot_done)
    );

    always #5 clock = ~clock;

    integer i;
    integer cycle;
    reg failed;

    // {addr, value} pairs for the same program sim/subleq_tb.v loads.
    reg [15:0] prog_addr [0:12];
    reg [15:0] prog_data [0:12];

    initial begin
        failed = 0;

        prog_addr[0] = 16'd0;   prog_data[0] = 16'd10;
        prog_addr[1] = 16'd1;   prog_data[1] = 16'd11;
        prog_addr[2] = 16'd2;   prog_data[2] = 16'd100;
        prog_addr[3] = 16'd3;   prog_data[3] = 16'd12;
        prog_addr[4] = 16'd4;   prog_data[4] = 16'd12;
        prog_addr[5] = 16'd5;   prog_data[5] = 16'd6;
        prog_addr[6] = 16'd6;   prog_data[6] = 16'd13;
        prog_addr[7] = 16'd7;   prog_data[7] = 16'd13;
        prog_addr[8] = 16'd8;   prog_data[8] = 16'd6;
        prog_addr[9] = 16'd10;  prog_data[9] = 16'd3;
        prog_addr[10] = 16'd11; prog_data[10] = 16'd8;
        prog_addr[11] = 16'd12; prog_data[11] = 16'd5;
        prog_addr[12] = 16'd13; prog_data[12] = 16'd0;

        reset = 1;
        repeat (3) @(posedge clock);
        #1;
        reset = 0;

        // Load the first half of the words through the real ports,
        // one per cycle, while booted == 0.
        for (i = 0; i < 6; i = i + 1) begin
            load_addr = prog_addr[i];
            load_data = prog_data[i];
            load_en = 1'b1;
            @(posedge clock);
            #1;
        end
        load_en = 1'b0;

        // Idle gap in the MIDDLE of loading, memory only half-written
        // and boot_done still low: step/refill must NOT start running
        // on this still-incomplete program.
        repeat (3) @(posedge clock);
        #1;
        if (dut.pc !== 16'd0 || dut.__cont_step !== 3'd0) begin
            $display("FAIL: CPU started running on a half-loaded program (pc=%0d, cont=%0d)", dut.pc, dut.__cont_step);
            failed = 1;
        end

        // Load the rest of the words.
        for (i = 6; i < 13; i = i + 1) begin
            load_addr = prog_addr[i];
            load_data = prog_data[i];
            load_en = 1'b1;
            @(posedge clock);
            #1;
        end
        load_en = 1'b0;

        // pc must still be at its reset value: the CPU rules never
        // fired during the whole load phase.
        if (dut.pc !== 16'd0) begin
            $display("FAIL: pc moved during loading (booted interlock did not hold), got %0d", dut.pc);
            failed = 1;
        end

        // A few idle cycles with boot_done still low: still must not run.
        repeat (2) @(posedge clock);
        #1;
        if (dut.pc !== 16'd0) begin
            $display("FAIL: pc moved before boot_done was asserted, got %0d", dut.pc);
            failed = 1;
        end

        // Pulse boot_done for one cycle.
        boot_done = 1'b1;
        @(posedge clock);
        #1;
        boot_done = 1'b0;

        // Two instructions to reach the halt (6 cycles each) plus
        // margin for settling.
        for (cycle = 0; cycle < 40; cycle = cycle + 1) begin
            @(posedge clock);
        end

        $display("final: pc=%0d ir=%0d mem[11]=%0d", dut.pc, dut.ir, dut.m_ext.Memory[11]);

        if (dut.pc !== 16'd6) begin
            $display("FAIL: expected pc to settle at the halt address 6, got %0d", dut.pc);
            failed = 1;
        end
        if (dut.m_ext.Memory[11] !== 16'd5) begin
            $display("FAIL: expected mem[11] == 8 - 3 == 5, got %0d", dut.m_ext.Memory[11]);
            failed = 1;
        end

        if (failed) begin
            $display("SIMULATION FAILED");
        end else begin
            $display("SIMULATION PASSED");
        end
        $finish;
    end
endmodule
