// Icarus Verilog testbench for the SUBLEQ milestone, driving the
// firtool-generated Verilog directly (`trace examples/subleq.tr --lower
// | trace --firrtl | firtool --disable-opt`).
//
// The `Subleq` module has no ports beyond clock/reset (the language has
// no `input`/`output` concept yet), so this testbench reaches into the
// design with hierarchical paths (`dut.pc`, `dut.m_ext.Memory[i]`)
// instead of driving real ports. Icarus allows this with no special
// compile flags. `m_ext`/`Memory` are firtool's own generated names for
// the `mem m` declaration (one instantiated `m_4096x16` submodule, a
// `Memory` register array inside it) — see sim/README.md if firtool
// ever renames them.
//
// Program (SUBLEQ: mem[b] -= mem[a]; if result <= 0 goto c else pc+3):
//   addr 0: subleq 10, 11, 100   -- mem[11] -= mem[10]; expect > 0, so
//                                    falls through to addr 3, NOT to
//                                    100 (a distinct, wrong address —
//                                    if the branch fires when it
//                                    shouldn't, execution diverges
//                                    somewhere else instead of by luck
//                                    landing on the right code)
//   addr 3: subleq 12, 12, 6     -- mem[12] -= mem[12] = 0 <= 0 always:
//                                    the standard SUBLEQ "unconditional
//                                    jump" idiom, always taken
//   addr 6: subleq 13, 13, 6     -- self-loop forever: halt
//   addr 10: 3                   -- operand a
//   addr 11: 8                   -- operand b; expect 8-3=5 afterward
//   addr 12: 5                   -- scratch (value irrelevant)
//   addr 13: 0                   -- scratch (value irrelevant)
//
// Expected outcome: pc settles at 6 and stays there; mem[11] == 5.
// If the conditional branch logic were wrong (e.g. always taken, or
// the mux picked the wrong operand), pc would instead wander from the
// garbage instruction at address 100 and never settle at 6.
module subleq_tb;
    reg clock = 0;
    reg reset = 1;

    Subleq dut (
        .clock(clock),
        .reset(reset)
    );

    always #5 clock = ~clock;

    integer i;
    integer cycle;
    reg failed;

    initial begin
        failed = 0;

        for (i = 0; i < 4096; i = i + 1) begin
            dut.m_ext.Memory[i] = 16'd0;
        end
        dut.m_ext.Memory[0] = 16'd10;
        dut.m_ext.Memory[1] = 16'd11;
        dut.m_ext.Memory[2] = 16'd100;
        dut.m_ext.Memory[3] = 16'd12;
        dut.m_ext.Memory[4] = 16'd12;
        dut.m_ext.Memory[5] = 16'd6;
        dut.m_ext.Memory[6] = 16'd13;
        dut.m_ext.Memory[7] = 16'd13;
        dut.m_ext.Memory[8] = 16'd6;
        dut.m_ext.Memory[10] = 16'd3;
        dut.m_ext.Memory[11] = 16'd8;
        dut.m_ext.Memory[12] = 16'd5;
        dut.m_ext.Memory[13] = 16'd0;

        reset = 1;
        repeat (3) @(posedge clock);
        reset = 0;

        // Two instructions to reach the halt (6 cycles each in the
        // current lowering) plus margin for reset and settling.
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
