#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static int rt_sigprocmask(int how, const unsigned long *set,
                          unsigned long *oldset)
{
    return (int)syscall(SYS_rt_sigprocmask, how, set, oldset,
                        sizeof(unsigned long));
}

int main(void)
{
    const unsigned long sigchld = 1UL << (SIGCHLD - 1);
    unsigned long empty = 0;
    unsigned long inout = ~0UL;
    unsigned long observed = ~0UL;

    if (rt_sigprocmask(SIG_SETMASK, &empty, NULL) != 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL reset errno=%d\n", errno);
        return 1;
    }

    /*
     * Linux snapshots the rt_sigprocmask input before copying out the old
     * mask. BusyBox ash relies on that syscall behavior to block signals
     * before sigsuspend while retaining the previous mask in place.
     */
    if (rt_sigprocmask(SIG_SETMASK, &inout, &inout) != 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL set errno=%d\n", errno);
        return 1;
    }
    if (inout != 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL old-mask value=%#lx\n", inout);
        return 1;
    }

    if (rt_sigprocmask(SIG_SETMASK, NULL, &observed) != 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL query errno=%d\n", errno);
        return 1;
    }
    if ((observed & sigchld) == 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL new-mask value=%#lx\n", observed);
        return 1;
    }

    if (rt_sigprocmask(SIG_SETMASK, &empty, NULL) != 0) {
        printf("CI_SIGNAL_MASK_ALIAS_FAIL restore errno=%d\n", errno);
        return 1;
    }

    puts("CI_SIGNAL_MASK_ALIAS_PASS");
    return 0;
}
