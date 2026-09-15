#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

/*
 * Differential coverage for the keyring and getrandom entry points.
 *
 * Every assertion below is a rule that the Linux v7.2.3 sources state
 * unconditionally, so it holds on both TheKernel and the reference kernel:
 *
 *   getrandom(2)  drivers/char/random.c
 *                 - an unknown flag, and GRND_INSECURE|GRND_RANDOM together,
 *                   are -EINVAL before any user access
 *                 - a zero length never touches the buffer and never fails on
 *                   the address, but the address must still be a user address
 *                 - an unreadable destination is -EFAULT
 *                 - GRND_INSECURE is filled without waiting for the CRNG
 *   add_key(2)    security/keys/keyctl.c
 *                 - plen > 1024*1024-1 is -EINVAL before any user access
 *                 - the type name is empty (-EINVAL), dot-prefixed (-EPERM)
 *                   or unknown (-ENODEV)
 *                 - a private "keyring.*" description is -EPERM
 *                 - a "user" payload must be 1..=32767 bytes
 *   request_key(2) security/keys/keyctl.c
 *                 - a NULL callout_info with no cached key is -ENOKEY and
 *                   never starts an upcall
 *                 - the "keyring" type is never constructed by an upcall
 *                   (-EPERM) even when a callout is supplied
 *                 - an unregistered type is -ENOKEY, not -ENODEV: the
 *                   syscall propagates `key_type_lookup()` unchanged
 *   keyctl(2)     security/keys/keyctl.c, security/keys/keyring.c
 *                 - KEYCTL_CAPABILITIES returns 2 and clears any tail
 *                 - KEYCTL_ASSUME_AUTHORITY rejects every negative serial
 *                 - KEYCTL_REJECT validates its error code before it looks
 *                   for an assumed authority
 *                 - KEYCTL_READ reports a failed lookup as -ENOKEY, lets a
 *                   keyring only be read in whole serial words, and answers a
 *                   buffer that cannot hold the whole result with the full
 *                   length but no data
 *                 - KEYCTL_RESTRICT_KEYRING reports an unknown key type as
 *                   -ENOKEY and a type without a restriction parser as
 *                   -ENOENT
 *                 - KEYCTL_UPDATE is -EINVAL above one page
 *                 - KEYCTL_JOIN_SESSION_KEYRING rejects an empty name
 */

#define KEY_SPEC_PROCESS_KEYRING (-2)

#define KEYCTL_JOIN_SESSION_KEYRING 1
#define KEYCTL_UPDATE 2
#define KEYCTL_READ 11
#define KEYCTL_ASSUME_AUTHORITY 16
#define KEYCTL_REJECT 19
#define KEYCTL_RESTRICT_KEYRING 29
#define KEYCTL_CAPABILITIES 31

#define GRND_NONBLOCK 0x0001
#define GRND_RANDOM 0x0002
#define GRND_INSECURE 0x0004

#define PAYLOAD_BYTES (1 << 20)

static unsigned char payload[PAYLOAD_BYTES];

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_KEYRING_RANDOM_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected) {
    fprintf(stderr,
            "THEKERNEL_KEYRING_RANDOM_FAIL %s actual=%ld expected=%ld\n", stage,
            actual, expected);
    return 1;
}

static int expect_errno(const char *stage, long result, int expected) {
    if (result != -1 || errno != expected) {
        fprintf(stderr,
                "THEKERNEL_KEYRING_RANDOM_FAIL %s result=%ld errno=%d "
                "expected=%d\n",
                stage, result, errno, expected);
        return 1;
    }
    return 0;
}

static long do_getrandom(void *buffer, size_t length, unsigned int flags) {
    return syscall(SYS_getrandom, buffer, length, flags);
}

static long do_add_key(const char *type, const char *description,
                       const void *data, size_t length, int keyring) {
    return syscall(SYS_add_key, type, description, data, (long)length,
                   (long)keyring);
}

static long do_request_key(const char *type, const char *description,
                           const char *callout, int keyring) {
    return syscall(SYS_request_key, type, description, callout, (long)keyring);
}

static long do_keyctl(long option, unsigned long arg2, unsigned long arg3,
                      unsigned long arg4, unsigned long arg5) {
    return syscall(SYS_keyctl, option, arg2, arg3, arg4, arg5);
}

/* getrandom: validation precedes any user access, and GRND_INSECURE never
 * waits for the CRNG, so none of these depend on the boot entropy state. */
static int test_getrandom_validation(void) {
    unsigned char buffer[64];
    memset(buffer, 0, sizeof(buffer));

    errno = 0;
    if (expect_errno("getrandom-unknown-flag", do_getrandom(buffer, 16, 0x8),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("getrandom-random-and-insecure",
                     do_getrandom(buffer, 16, GRND_RANDOM | GRND_INSECURE),
                     EINVAL))
        return 1;
    if (do_getrandom(NULL, 0, 0) != 0)
        return fail("getrandom-null-zero-length");
    /* A zero length still has to name a user address, but any address below
     * the user ceiling is accepted, including an unmapped one. */
    if (do_getrandom((void *)1, 0, 0) != 0)
        return fail("getrandom-unmapped-zero-length");
    errno = 0;
    if (expect_errno("getrandom-kernel-address-zero-length",
                     do_getrandom((void *)(uintptr_t)0x8000000000000000ULL, 0,
                                  0),
                     EFAULT))
        return 1;
    errno = 0;
    if (expect_errno("getrandom-null-destination",
                     do_getrandom(NULL, 16, GRND_INSECURE), EFAULT))
        return 1;
    return 0;
}

static int test_getrandom_insecure_delivery(void) {
    unsigned char buffer[64];
    memset(buffer, 0, sizeof(buffer));
    if (do_getrandom(buffer, sizeof(buffer), GRND_INSECURE) !=
        (long)sizeof(buffer))
        return fail("getrandom-insecure-full");
    if (do_getrandom(buffer, 1, GRND_INSECURE) != 1)
        return fail("getrandom-insecure-one");
    if (do_getrandom(buffer, 16, GRND_INSECURE | GRND_NONBLOCK) != 16)
        return fail("getrandom-insecure-nonblock");
    return 0;
}

static int test_add_key_validation(void) {
    long key;

    errno = 0;
    if (expect_errno("add-key-one-megabyte",
                     do_add_key("user", "kr-desc", payload, PAYLOAD_BYTES,
                                KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("add-key-empty-payload",
                     do_add_key("user", "kr-desc", payload, 0,
                                KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("add-key-oversized-user-payload",
                     do_add_key("user", "kr-desc", payload, 32768,
                                KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("add-key-unknown-type",
                     do_add_key("kr-bogus-type", "kr-desc", payload, 1,
                                KEY_SPEC_PROCESS_KEYRING),
                     ENODEV))
        return 1;
    errno = 0;
    if (expect_errno("add-key-empty-type",
                     do_add_key("", "kr-desc", payload, 1,
                                KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("add-key-dot-type",
                     do_add_key(".user", "kr-desc", payload, 1,
                                KEY_SPEC_PROCESS_KEYRING),
                     EPERM))
        return 1;
    errno = 0;
    if (expect_errno("add-key-private-keyring",
                     do_add_key("keyring", ".kr-private", payload, 0,
                                KEY_SPEC_PROCESS_KEYRING),
                     EPERM))
        return 1;
    key = do_add_key("user", "kr-alpha", payload, 8, KEY_SPEC_PROCESS_KEYRING);
    if (key <= 0)
        return fail("add-key-user");
    if (do_add_key("keyring", "kr-ring", payload, 0,
                   KEY_SPEC_PROCESS_KEYRING) <= 0)
        return fail("add-key-keyring");
    return 0;
}

static int test_request_key_callout_rules(void) {
    long key = do_add_key("user", "kr-alpha", payload, 8,
                          KEY_SPEC_PROCESS_KEYRING);
    if (key <= 0)
        return fail("request-key-seed");
    /* A cached key is returned even though no callout was supplied. */
    if (do_request_key("user", "kr-alpha", NULL, KEY_SPEC_PROCESS_KEYRING) !=
        key) {
        errno = EPROTO;
        return fail("request-key-cached");
    }
    /* No cached key and no callout: -ENOKEY, never an upcall. */
    errno = 0;
    if (expect_errno("request-key-null-callout",
                     do_request_key("user", "kr-absent", NULL,
                                    KEY_SPEC_PROCESS_KEYRING),
                     ENOKEY))
        return 1;
    errno = 0;
    if (expect_errno("request-key-null-callout-keyring-type",
                     do_request_key("keyring", "kr-absent-ring", NULL,
                                    KEY_SPEC_PROCESS_KEYRING),
                     ENOKEY))
        return 1;
    /* A callout is supplied, but a keyring is never built by the upcall. */
    errno = 0;
    if (expect_errno("request-key-keyring-callout",
                     do_request_key("keyring", "kr-absent-ring", "callout",
                                    KEY_SPEC_PROCESS_KEYRING),
                     EPERM))
        return 1;
    /* `request_key(2)` propagates `key_type_lookup()` unchanged, so an
     * unregistered type is -ENOKEY (security/keys/key.c). */
    errno = 0;
    if (expect_errno("request-key-unknown-type",
                     do_request_key("kr-bogus-type", "kr-absent", "callout",
                                    KEY_SPEC_PROCESS_KEYRING),
                     ENOKEY))
        return 1;
    return 0;
}

static int test_keyctl_read_rules(void) {
    unsigned char buffer[64];
    long ring = do_add_key("keyring", "kr-read-ring", payload, 0,
                           KEY_SPEC_PROCESS_KEYRING);
    long user = do_add_key("user", "kr-read-user", payload, 8,
                           KEY_SPEC_PROCESS_KEYRING);
    if (ring <= 0 || user <= 0)
        return fail("keyctl-read-seed");

    /* An empty keyring reports no serials; the size query ignores the
     * buffer entirely. */
    if (do_keyctl(KEYCTL_READ, (unsigned long)ring, 0, 0, 0) != 0)
        return fail_value("keyctl-read-empty-ring",
                          do_keyctl(KEYCTL_READ, (unsigned long)ring, 0, 0, 0),
                          0);
    /* A keyring byte count that is not a whole number of serials. */
    errno = 0;
    if (expect_errno("keyctl-read-ring-partial-word",
                     do_keyctl(KEYCTL_READ, (unsigned long)ring,
                               (unsigned long)buffer, 6, 0),
                     EINVAL))
        return 1;
    if (do_keyctl(KEYCTL_READ, (unsigned long)ring, (unsigned long)buffer, 4,
                  0) != 0)
        return fail("keyctl-read-ring-word");

    /* A payload read reports the full length; the data only arrives when it
     * fits whole, because `keyctl_read_key()` stages the read method's output
     * in a kernel buffer and skips `copy_to_user()` when ret > buflen. */
    memset(buffer, 0xcc, sizeof(buffer));
    if (do_keyctl(KEYCTL_READ, (unsigned long)user, (unsigned long)buffer, 4,
                  0) != 8)
        return fail("keyctl-read-user-short");
    for (size_t index = 0; index < sizeof(buffer); ++index) {
        if (buffer[index] != 0xcc)
            return fail_value("keyctl-read-user-short-bytes", buffer[index],
                              0xcc);
    }
    memset(buffer, 0, sizeof(buffer));
    if (do_keyctl(KEYCTL_READ, (unsigned long)user, (unsigned long)buffer, 64,
                  0) != 8)
        return fail("keyctl-read-user-whole");
    if (memcmp(buffer, payload, 8) != 0)
        return fail("keyctl-read-user-whole-bytes");
    if (do_keyctl(KEYCTL_READ, (unsigned long)user, 0, 64, 0) != 8)
        return fail("keyctl-read-user-size-only");
    if (do_keyctl(KEYCTL_READ, (unsigned long)user, 0, 0, 0) != 8)
        return fail("keyctl-read-user-no-buffer");

    /* A lookup failure is reported as -ENOKEY, whatever went wrong. */
    errno = 0;
    if (expect_errno("keyctl-read-missing-serial",
                     do_keyctl(KEYCTL_READ, 0x7ffffff0UL,
                               (unsigned long)buffer, 8, 0),
                     ENOKEY))
        return 1;
    errno = 0;
    if (expect_errno("keyctl-read-zero-serial",
                     do_keyctl(KEYCTL_READ, 0, (unsigned long)buffer, 8, 0),
                     ENOKEY))
        return 1;
    return 0;
}

static int test_keyctl_validation(void) {
    unsigned char capabilities[8];
    long ring = do_add_key("keyring", "kr-limit-ring", payload, 0,
                           KEY_SPEC_PROCESS_KEYRING);
    if (ring <= 0)
        return fail("keyctl-validation-seed");

    memset(capabilities, 0, sizeof(capabilities));
    if (do_keyctl(KEYCTL_CAPABILITIES, (unsigned long)capabilities, 2, 0, 0) !=
        2)
        return fail("keyctl-capabilities-length");
    if ((capabilities[0] & 0xe1) != 0xe1)
        return fail_value("keyctl-capabilities-word0",
                          capabilities[0] & 0xe1, 0xe1);
    if ((capabilities[1] & 0x03) != 0x03)
        return fail_value("keyctl-capabilities-word1",
                          capabilities[1] & 0x03, 0x03);
    /* A longer buffer is filled to the end: the two capability bytes first,
     * then zeroes. */
    memset(capabilities, 0xaa, sizeof(capabilities));
    if (do_keyctl(KEYCTL_CAPABILITIES, (unsigned long)capabilities,
                  sizeof(capabilities), 0, 0) != 2)
        return fail("keyctl-capabilities-long");
    for (size_t index = 2; index < sizeof(capabilities); ++index) {
        if (capabilities[index] != 0)
            return fail_value("keyctl-capabilities-tail", capabilities[index],
                              0);
    }

    /* Every negative serial is rejected, including the KEY_SPEC_* ids. */
    errno = 0;
    if (expect_errno("keyctl-assume-authority-minus-one",
                     do_keyctl(KEYCTL_ASSUME_AUTHORITY,
                               (unsigned long)-1L, 0, 0, 0),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("keyctl-assume-authority-spec",
                     do_keyctl(KEYCTL_ASSUME_AUTHORITY,
                               (unsigned long)-3L, 0, 0, 0),
                     EINVAL))
        return 1;

    /* The rejection error code is validated before the authority lookup. */
    errno = 0;
    if (expect_errno("keyctl-reject-zero-error",
                     do_keyctl(KEYCTL_REJECT, 1, 0, 0,
                               (unsigned long)KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("keyctl-reject-max-errno",
                     do_keyctl(KEYCTL_REJECT, 1, 0, 5000,
                               (unsigned long)KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;
    errno = 0;
    if (expect_errno("keyctl-reject-restart-block",
                     do_keyctl(KEYCTL_REJECT, 1, 0, 512,
                               (unsigned long)KEY_SPEC_PROCESS_KEYRING),
                     EINVAL))
        return 1;

    /* An unregistered key type cannot be restricted (-ENOKEY, the raw
     * `key_type_lookup()` result); a registered one has no restriction
     * parser in this kernel (-ENOENT). */
    errno = 0;
    if (expect_errno("keyctl-restrict-unknown-type",
                     do_keyctl(KEYCTL_RESTRICT_KEYRING, (unsigned long)ring,
                               (unsigned long)(uintptr_t)"kr-bogus-type",
                               (unsigned long)(uintptr_t)"type", 0),
                     ENOKEY))
        return 1;
    errno = 0;
    if (expect_errno("keyctl-restrict-no-backend",
                     do_keyctl(KEYCTL_RESTRICT_KEYRING, (unsigned long)ring,
                               (unsigned long)(uintptr_t)"user",
                               (unsigned long)(uintptr_t)"type", 0),
                     ENOENT))
        return 1;

    /* Only one page of payload can be updated at a time. */
    errno = 0;
    if (expect_errno("keyctl-update-over-page",
                     do_keyctl(KEYCTL_UPDATE, 1, (unsigned long)payload, 4097UL, 0),
                     EINVAL))
        return 1;

    /* An empty session keyring name has no key description to allocate. */
    errno = 0;
    if (expect_errno("keyctl-join-empty-name",
                     do_keyctl(KEYCTL_JOIN_SESSION_KEYRING,
                               (unsigned long)(uintptr_t)"", 0, 0, 0), EINVAL))
        return 1;
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    memset(payload, 0x41, sizeof(payload));

    puts("THEKERNEL_ABI_CASE keyring-random.portable-differential");
    if (test_getrandom_validation())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "GETRANDOM_VALIDATION pass");
    if (test_getrandom_insecure_delivery())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "GETRANDOM_INSECURE_DELIVERY pass");
    if (test_add_key_validation())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "ADD_KEY_VALIDATION pass");
    if (test_request_key_callout_rules())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "REQUEST_KEY_CALLOUT pass");
    if (test_keyctl_read_rules())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "KEYCTL_READ_RULES pass");
    if (test_keyctl_validation())
        return 1;
    puts("THEKERNEL_ABI_ASSERT keyring-random.portable-differential "
         "KEYCTL_VALIDATION pass");

    puts("THEKERNEL_KEYRING_RANDOM_OK");
    puts("THEKERNEL_ABI_RESULT keyring-random.portable-differential pass");
    return 0;
}
