/*
# Context switch

void swtch(struct context *old, struct context *new);

Save current registers in old. Load from new.
*/

use core::arch::global_asm;
