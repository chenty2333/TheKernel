#define _GNU_SOURCE

#include <features.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#if defined(__GLIBC__)
typedef int mmsg_flags_t;
#else
typedef unsigned int mmsg_flags_t;
#endif

int sendmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, mmsg_flags_t flags)
{
    return syscall(SYS_sendmmsg, sockfd, msgvec, vlen, flags);
}

int recvmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, mmsg_flags_t flags,
             struct timespec *timeout)
{
    return syscall(SYS_recvmmsg, sockfd, msgvec, vlen, flags, timeout);
}
