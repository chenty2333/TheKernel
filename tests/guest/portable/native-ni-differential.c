#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* x86_64 Linux syscall slots that are permanently wired to sys_ni_syscall.
 * Invoke them by their raw ABI numbers: none accepts arguments or has side
 * effects when the NI contract is honored. */
struct native_ni_slot {
    long number;
    const char *assertion;
};

static const struct native_ni_slot native_ni_slots[] = {
    {134, "NR_134_USELIB"},
    {156, "NR_156_SYSCTL"},
    {174, "NR_174_CREATE_MODULE"},
    {177, "NR_177_GET_KERNEL_SYMS"},
    {178, "NR_178_QUERY_MODULE"},
    {180, "NR_180_NFSSERVCTL"},
    {181, "NR_181_GETPMSG"},
    {182, "NR_182_PUTPMSG"},
    {183, "NR_183_AFS_SYSCALL"},
    {184, "NR_184_TUXCALL"},
    {185, "NR_185_SECURITY"},
    {205, "NR_205_SET_THREAD_AREA"},
    {211, "NR_211_GET_THREAD_AREA"},
    {212, "NR_212_LOOKUP_DCOOKIE"},
    {214, "NR_214_EPOLL_CTL_OLD"},
    {215, "NR_215_EPOLL_WAIT_OLD"},
    {236, "NR_236_VSERVER"},
};

static int expect_enosys(long number, const char *assertion) {
    errno = 0;
    long result = syscall(number, 0L, 0L, 0L, 0L, 0L, 0L);
    if (result == -1 && errno == ENOSYS) {
        printf("THEKERNEL_ABI_ASSERT native-ni.fixed-slots %s enosys\n",
               assertion);
        return 0;
    }

    fprintf(stderr,
            "THEKERNEL_NATIVE_NI_FAIL assertion=%s nr=%ld result=%ld errno=%d (%s)\n",
            assertion, number, result, errno, strerror(errno));
    return 1;
}

int main(void) {
    for (size_t index = 0;
         index < sizeof(native_ni_slots) / sizeof(native_ni_slots[0]);
         ++index) {
        if (index == 0) {
            puts("THEKERNEL_ABI_CASE native-ni.fixed-slots");
        }
        if (expect_enosys(native_ni_slots[index].number,
                         native_ni_slots[index].assertion) != 0) {
            return 1;
        }
    }

    /* Deliberately outside the x86_64 syscall table; keep this oracle distinct
     * from the fixed native-NI slots above. */
    if (expect_enosys(1024, "OUT_OF_RANGE_1024") != 0) {
        return 1;
    }

    puts("THEKERNEL_NATIVE_NI_OK");
    puts("THEKERNEL_ABI_RESULT native-ni.fixed-slots enosys");
    return 0;
}
