#define _GNU_SOURCE

#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

int sendmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, int flags)
{
    return syscall(SYS_sendmmsg, sockfd, msgvec, vlen, flags);
}

int recvmmsg(int sockfd, struct mmsghdr *msgvec, unsigned int vlen, int flags,
             struct timespec *timeout)
{
    return syscall(SYS_recvmmsg, sockfd, msgvec, vlen, flags, timeout);
}
