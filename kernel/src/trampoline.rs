//! ユーザ空間とカーネル空間を切り替える処理
//!
//! この処理はユーザ空間とカーネル空間で同一の仮想アドレスにマップされるため、
//! ページテーブルを切り替えても処理が継続する。
//!
//! この処理の命令列はlinker.ldによってページ境界にアラインされる。
//!
//!

use core::arch::global_asm;

global_asm!(
    r#"
.section trampsec
.global trampoline
trampoline:
.align 4
.global user_trap_handler
user_trap_handler:
    # trap.cの処理によって、この処理がstvec(割り込みハンドラ)に設定される。
    # ユーザー空間からの割り込みを処理する。
    # スーパーバイザモードではあるが、ユーザページテーブルを用いる。
    #
    # sscratchは、プロセスのp->trapframeがユーザ空間にマップされる場所、TRAPFRAMEを指す。
    #

    # a0とsscratch(TRAPFRAME)を交換する
    csrrw a0, sscratch, a0

    # TRAPFRAMEにユーザのレジスタを格納
    sd ra, 40(a0)
    sd sp, 48(a0)
    sd gp, 56(a0)
    sd tp, 64(a0)
    sd t0, 72(a0)
    sd t1, 80(a0)
    sd t2, 88(a0)
    sd s0, 96(a0)
    sd s1, 104(a0)
    sd a1, 120(a0)
    sd a2, 128(a0)
    sd a3, 136(a0)
    sd a4, 144(a0)
    sd a5, 152(a0)
    sd a6, 160(a0)
    sd a7, 168(a0)
    sd s2, 176(a0)
    sd s3, 184(a0)
    sd s4, 192(a0)
    sd s5, 200(a0)
    sd s6, 208(a0)
    sd s7, 216(a0)
    sd s8, 224(a0)
    sd s9, 232(a0)
    sd s10, 240(a0)
    sd s11, 248(a0)
    sd t3, 256(a0)
    sd t4, 264(a0)
    sd t5, 272(a0)
    sd t6, 280(a0)

    # sscratchに一時的に保存されたa0の値も格納する
    # (p->trapframe->a0)
    csrr t0, sscratch
    sd t0, 112(a0)

    # p->trapframe->kernel_spからカーネルのスタックポインタを復元 
    ld sp, 8(a0)

    # スレッドポインタに現在のhartidを持たせる
    # (p->trapframe->kernel_hartid)
    ld tp, 32(a0)

    # usertrap関数のアドレスを読み込み
    # (p->trapframe->kernel_trap)
    ld t0, 16(a0)

    # カーネルのページテーブルを復元
    # (p->trapframe->kernel_satp)
    ld t1, 0(a0)
    csrw satp, t1
    sfence.vma zero, zero

    # 復元されたカーネルのページテーブルは、
    # p->trapframeを特別にマップしていないため、
    # 以後アクセス不能

    # usertrapにジャンプする。
    # 帰っては来ない。
    jr t0

.global kernel_to_user
kernel_to_user:
    # kernel_to_user(TRAPFRAME, pagetable)
    # カーネルからユーザへ切り替える。
    # usertrapretによって呼び出される。
    # 引数:
    # a0: TRAPFRAME, in user page table.
    # a1: user page table, for satp.

    # switch to the user page table.
    csrw satp, a1
    sfence.vma zero, zero

    # put the saved user a0 in sscratch, so we
    # can swap it with our a0 (TRAPFRAME) in the last step.
    ld t0, 112(a0)
    csrw sscratch, t0

    # restore all but a0 from TRAPFRAME
    ld ra, 40(a0)
    ld sp, 48(a0)
    ld gp, 56(a0)
    ld tp, 64(a0)
    ld t0, 72(a0)
    ld t1, 80(a0)
    ld t2, 88(a0)
    ld s0, 96(a0)
    ld s1, 104(a0)
    ld a1, 120(a0)
    ld a2, 128(a0)
    ld a3, 136(a0)
    ld a4, 144(a0)
    ld a5, 152(a0)
    ld a6, 160(a0)
    ld a7, 168(a0)
    ld s2, 176(a0)
    ld s3, 184(a0)
    ld s4, 192(a0)
    ld s5, 200(a0)
    ld s6, 208(a0)
    ld s7, 216(a0)
    ld s8, 224(a0)
    ld s9, 232(a0)
    ld s10, 240(a0)
    ld s11, 248(a0)
    ld t3, 256(a0)
    ld t4, 264(a0)
    ld t5, 272(a0)
    ld t6, 280(a0)

    # restore user a0, and save TRAPFRAME in sscratch
    csrrw a0, sscratch, a0

    # return to user mode and user pc.
    # usertrapret() set up sstatus and sepc.
    sret
"#
);
