//! 最初に実行される処理
//!
//! `qemu -kernel`によってカーネルは0x80000000番地にロードされ、各CPUはそこにジャンプする。
//! linker.ldによって、この処理のコードは0x80000000番地に配置される。
//!
//! `sp = stack0 + (hartid * 4096)`
//!
//! この処理は、各CPUにそれぞれ4096バイトのスタック領域を割り当て、start関数を呼び出す。

use core::arch::global_asm;

global_asm!(
    r#"
# FIXME: LLVMがRISC-Vの拡張機能を認識していない？
# https://github.com/rust-lang/rust/issues/80608
.attribute arch, "rv64gc"

.section .text
.global _entry
_entry:
    # a0 = 4096 * (mhartid + 1)
    csrr a1, mhartid
    addi a1, a1, 1
    li a0, 1024*4
    mul a0, a0, a1

    # sp = stack0 + a0
    la sp, stack0
    add sp, sp, a0

    # start()
    call start
spin:
    j spin
"#
);
