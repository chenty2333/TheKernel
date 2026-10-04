/* Run exactly the guest mutation regression on the pinned Linux oracle too. */
#define main ptrace_register_case
#include "ptrace-registers.c"
#undef main

int main(void) {
    puts("THEKERNEL_ABI_CASE ptrace-registers.raw-differential");
    if (ptrace_register_case() != 0) return 1;
    puts("THEKERNEL_ABI_ASSERT ptrace-registers.raw-differential REGISTER_AND_FP_MUTATION pass");
    puts("THEKERNEL_ABI_RESULT ptrace-registers.raw-differential pass");
    puts("THEKERNEL_PTRACE_REGISTERS_DIFFERENTIAL_OK");
    return 0;
}
