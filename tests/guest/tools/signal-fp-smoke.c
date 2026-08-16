#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <ucontext.h>
#include <unistd.h>
#include <asm/ucontext.h>

#if !defined(__x86_64__)
#error "the signal FP smoke is x86_64-only"
#endif

#define FPSTATE_CW_OFFSET 0U
#define FPSTATE_MXCSR_OFFSET 24U
#define FPSTATE_XMM15_OFFSET 400U
#define FPSTATE_LEGACY_BYTES 512U
#define FPSTATE_XMM_BYTES 16U
#define REQUIRED_UC_FLAGS (UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS)
#define INITIAL_MXCSR UINT32_C(0x1f80)

_Static_assert(FPSTATE_XMM15_OFFSET + FPSTATE_XMM_BYTES <=
                   FPSTATE_LEGACY_BYTES,
               "XMM15 must be in the legacy FXSAVE area");

static const unsigned char state_initial_xmm[FPSTATE_XMM_BYTES] = {
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
    0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
};
static const unsigned char state_entry_xmm[FPSTATE_XMM_BYTES] = {0};
static const unsigned char state_outer_frame_xmm[FPSTATE_XMM_BYTES] = {
    0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
};
static const unsigned char state_outer_live_xmm[FPSTATE_XMM_BYTES] = {
    0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
    0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40,
};
static const unsigned char state_inner_frame_xmm[FPSTATE_XMM_BYTES] = {
    0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50,
};
static const unsigned char state_inner_live_xmm[FPSTATE_XMM_BYTES] = {
    0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58,
    0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60,
};

enum {
    CW_INITIAL = 0x037f,
    CW_OUTER_FRAME = 0x077f,
    CW_OUTER_LIVE = 0x0f7f,
    CW_INNER_FRAME = 0x0b7f,
    CW_INNER_LIVE = 0x037f,
};

enum handler_failure {
    FAILURE_NONE = 0,
    FAILURE_OUTER_DEPTH = 1,
    FAILURE_INNER_DEPTH = 2,
    FAILURE_OUTER_FRAME = 3,
    FAILURE_INNER_FRAME = 4,
    FAILURE_NESTED_SEND = 5,
    FAILURE_NESTED_RESTORE = 6,
    FAILURE_OUTER_ENTRY = 7,
    FAILURE_INNER_ENTRY = 8,
};

static volatile sig_atomic_t handler_failure;
static volatile sig_atomic_t handler_depth;
static volatile sig_atomic_t nested_returned;
static pid_t self_pid;
static pid_t self_tid;

static uint16_t frame_load_cw(const volatile unsigned char *fpstate) {
    return (uint16_t)fpstate[FPSTATE_CW_OFFSET] |
           (uint16_t)((uint16_t)fpstate[FPSTATE_CW_OFFSET + 1U] << 8);
}

static uint32_t frame_load_mxcsr(const volatile unsigned char *fpstate) {
    return (uint32_t)fpstate[FPSTATE_MXCSR_OFFSET] |
           ((uint32_t)fpstate[FPSTATE_MXCSR_OFFSET + 1U] << 8) |
           ((uint32_t)fpstate[FPSTATE_MXCSR_OFFSET + 2U] << 16) |
           ((uint32_t)fpstate[FPSTATE_MXCSR_OFFSET + 3U] << 24);
}

static void frame_store_cw(volatile unsigned char *fpstate, uint16_t value) {
    fpstate[FPSTATE_CW_OFFSET] = (unsigned char)value;
    fpstate[FPSTATE_CW_OFFSET + 1U] = (unsigned char)(value >> 8);
}

static int frame_xmm_equal(const volatile unsigned char *fpstate,
                           const unsigned char expected[FPSTATE_XMM_BYTES]) {
    for (size_t index = 0; index < FPSTATE_XMM_BYTES; ++index) {
        if (fpstate[FPSTATE_XMM15_OFFSET + index] != expected[index]) {
            return 0;
        }
    }
    return 1;
}

static int xmm_equal(const unsigned char actual[FPSTATE_XMM_BYTES],
                     const unsigned char expected[FPSTATE_XMM_BYTES]) {
    for (size_t index = 0; index < FPSTATE_XMM_BYTES; ++index) {
        if (actual[index] != expected[index]) {
            return 0;
        }
    }
    return 1;
}

static void frame_store_xmm(volatile unsigned char *fpstate,
                            const unsigned char value[FPSTATE_XMM_BYTES]) {
    for (size_t index = 0; index < FPSTATE_XMM_BYTES; ++index) {
        fpstate[FPSTATE_XMM15_OFFSET + index] = value[index];
    }
}

/* Keep the state transition and signal syscall in one asm block. The signal
 * is delivered on the return-to-user boundary after SYS_tgkill, before the
 * caller can run its post-signal capture. */
__attribute__((noinline)) static long trigger_signal(
    int signo, const unsigned char set_xmm[FPSTATE_XMM_BYTES],
    uint16_t set_cw) {
    long result;
    __asm__ volatile(
        "movdqu %[set_xmm], %%xmm15\n\t"
        "fldcw %[set_cw]\n\t"
        "syscall\n\t"
        : [result] "=a"(result)
        : [syscall_number] "a"((long)SYS_tgkill),
          [pid] "D"((long)self_pid),
          [tid] "S"((long)self_tid),
          [signo] "d"((long)signo),
          [set_xmm] "m"(*set_xmm),
          [set_cw] "m"(set_cw)
        : "cc", "memory", "rcx", "r11", "xmm15");
    return result;
}

__attribute__((noinline)) static void capture_live_state(
    unsigned char observed_xmm[FPSTATE_XMM_BYTES], uint16_t *observed_cw,
    uint32_t *observed_mxcsr) {
    __asm__ volatile("movdqu %%xmm15, %0\n\tfnstcw %1\n\tstmxcsr %2"
                     : "=m"(*observed_xmm), "=m"(*observed_cw),
                       "=m"(*observed_mxcsr)
                     :
                     : "memory", "xmm15");
}

__attribute__((noinline)) static void set_live_state(
    const unsigned char xmm[FPSTATE_XMM_BYTES], uint16_t cw) {
    __asm__ volatile(
        "movdqu %[xmm], %%xmm15\n\t"
        "fldcw %[cw]\n\t"
        :
        : [xmm] "m"(*xmm), [cw] "m"(cw)
        : "memory", "xmm15");
}

static int frame_is_legacy_signal_context(const ucontext_t *context,
                                          const unsigned char expected_xmm[
                                              FPSTATE_XMM_BYTES],
                                          uint16_t expected_cw,
                                          uint32_t expected_mxcsr,
                                          uint64_t expected_oldmask) {
    if ((context->uc_flags & REQUIRED_UC_FLAGS) != REQUIRED_UC_FLAGS) {
        return 0;
    }
    if (context->uc_mcontext.fpregs == NULL ||
        ((uintptr_t)context->uc_mcontext.fpregs & 0xfU) != 0) {
        return 0;
    }

    /* REG_OLDMASK is the legacy sigcontext slot. The outer frame starts with
     * an empty mask; the nested frame inherits SIGUSR1's automatic mask. The
     * high selector in CSGSFS is the UC_SIGCONTEXT_SS value on x86_64. */
    uint64_t oldmask = (uint64_t)context->uc_mcontext.gregs[REG_OLDMASK];
    uint16_t ss = (uint16_t)((uint64_t)context->uc_mcontext.gregs[REG_CSGSFS] >>
                             48);
    if (oldmask != expected_oldmask || ss == 0 || (ss & 0x3U) != 0x3U) {
        return 0;
    }

    const volatile unsigned char *fpstate =
        (const volatile unsigned char *)(uintptr_t)context->uc_mcontext.fpregs;
    return frame_load_cw(fpstate) == expected_cw &&
           frame_load_mxcsr(fpstate) == expected_mxcsr &&
           frame_xmm_equal(fpstate, expected_xmm);
}

static void modify_legacy_signal_context(
    ucontext_t *context, const unsigned char frame_xmm[FPSTATE_XMM_BYTES],
    uint16_t frame_cw) {
    volatile unsigned char *fpstate =
        (volatile unsigned char *)(uintptr_t)context->uc_mcontext.fpregs;
    frame_store_cw(fpstate, frame_cw);
    frame_store_xmm(fpstate, frame_xmm);
}

static void signal_handler(int signo, siginfo_t *info, void *opaque_context) {
    unsigned char entry_xmm[FPSTATE_XMM_BYTES];
    uint16_t entry_cw = 0;
    uint32_t entry_mxcsr = 0;
    capture_live_state(entry_xmm, &entry_cw, &entry_mxcsr);
    (void)info;
    ucontext_t *context = (ucontext_t *)opaque_context;

    if (signo == SIGUSR1) {
        if (!xmm_equal(entry_xmm, state_entry_xmm) ||
            entry_cw != CW_INITIAL || entry_mxcsr != INITIAL_MXCSR) {
            handler_failure = FAILURE_OUTER_ENTRY;
            return;
        }
        if (handler_depth != 0) {
            handler_failure = FAILURE_OUTER_DEPTH;
            return;
        }
        handler_depth = 1;
        if (!frame_is_legacy_signal_context(context, state_initial_xmm,
                                            CW_INITIAL, INITIAL_MXCSR, 0)) {
            handler_failure = FAILURE_OUTER_FRAME;
            handler_depth = 0;
            return;
        }
        modify_legacy_signal_context(context, state_outer_frame_xmm,
                                     CW_OUTER_FRAME);

        unsigned char observed_xmm[FPSTATE_XMM_BYTES];
        uint16_t observed_cw = 0;
        uint32_t observed_mxcsr = 0;
        if (trigger_signal(SIGUSR2, state_outer_live_xmm, CW_OUTER_LIVE) !=
            0) {
            handler_failure = FAILURE_NESTED_SEND;
            handler_depth = 0;
            return;
        }
        capture_live_state(observed_xmm, &observed_cw, &observed_mxcsr);
        if (observed_cw != CW_INNER_FRAME ||
            observed_mxcsr != INITIAL_MXCSR ||
            !xmm_equal(observed_xmm, state_inner_frame_xmm)) {
            handler_failure = FAILURE_NESTED_RESTORE;
            handler_depth = 0;
            return;
        }
        nested_returned = 1;
        /* Deliberately leave a value in live state that differs from the
         * outer frame. The outer rt_sigreturn must restore the frame edit. */
        set_live_state(state_inner_live_xmm, CW_INNER_LIVE);
        handler_depth = 0;
        return;
    }

    if (signo == SIGUSR2) {
        if (!xmm_equal(entry_xmm, state_entry_xmm) ||
            entry_cw != CW_INITIAL || entry_mxcsr != INITIAL_MXCSR) {
            handler_failure = FAILURE_INNER_ENTRY;
            return;
        }
        if (handler_depth != 1) {
            handler_failure = FAILURE_INNER_DEPTH;
            return;
        }
        handler_depth = 2;
        if (!frame_is_legacy_signal_context(
                context, state_outer_live_xmm, CW_OUTER_LIVE, INITIAL_MXCSR,
                UINT64_C(1) << (SIGUSR1 - 1))) {
            handler_failure = FAILURE_INNER_FRAME;
            handler_depth = 1;
            return;
        }
        modify_legacy_signal_context(context, state_inner_frame_xmm,
                                     CW_INNER_FRAME);
        set_live_state(state_inner_live_xmm, CW_INNER_LIVE);
        handler_depth = 1;
    }
}

static int install_handler(int signo) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = signal_handler;
    action.sa_flags = SA_SIGINFO;
    if (sigemptyset(&action.sa_mask) != 0 ||
        sigaction(signo, &action, NULL) != 0) {
        return -1;
    }
    return 0;
}

int main(void) {
    self_pid = getpid();
    self_tid = (pid_t)syscall(SYS_gettid);
    if (self_tid <= 0 || install_handler(SIGUSR1) != 0 ||
        install_handler(SIGUSR2) != 0) {
        fprintf(stderr, "THEKERNEL_SIGNAL_FP_FAIL setup errno=%d (%s)\n", errno,
                strerror(errno));
        return EXIT_FAILURE;
    }

    unsigned char observed_xmm[FPSTATE_XMM_BYTES];
    uint16_t observed_cw = 0;
    uint32_t observed_mxcsr = 0;
    if (trigger_signal(SIGUSR1, state_initial_xmm, CW_INITIAL) != 0) {
        fprintf(stderr, "THEKERNEL_SIGNAL_FP_FAIL signal errno=%d (%s)\n",
                errno, strerror(errno));
        return EXIT_FAILURE;
    }
    capture_live_state(observed_xmm, &observed_cw, &observed_mxcsr);
    if (handler_failure != FAILURE_NONE || handler_depth != 0 ||
        nested_returned == 0 || observed_cw != CW_OUTER_FRAME ||
        observed_mxcsr != INITIAL_MXCSR ||
        memcmp(observed_xmm, state_outer_frame_xmm, FPSTATE_XMM_BYTES) != 0) {
        fprintf(stderr,
                "THEKERNEL_SIGNAL_FP_FAIL restore handler_stage=%d nested=%d\n",
                handler_failure, nested_returned);
        return EXIT_FAILURE;
    }

    puts("THEKERNEL_SIGNAL_FP_OK");
    return EXIT_SUCCESS;
}
