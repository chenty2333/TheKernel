#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/inotify.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#define BAD ((void *)(uintptr_t)1)
static const char *active;
static void begin(const char *name) { active=name; printf("THEKERNEL_ABI_CASE %s\n",name); }
static void check(int ok,const char *name) { if(!ok) { fprintf(stderr,"THEKERNEL_FS_BOUNDARY_FAIL %s %s errno=%d\n",active,name,errno); exit(1); } }
static void mark(const char *name) { printf("THEKERNEL_ABI_ASSERT %s %s pass\n",active,name); }
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n",active); }
#define ERROR(call,err,name) do { errno=0; long rc=(call); check(rc==-1 && errno==(err),name); } while(0)
int main(void) {
    begin("flock.raw-differential");
    check(syscall(SYS_flock,-1,32)==0 && syscall(SYS_flock,-1,32|0x4000)==0,"mandatory"); mark("MANDATORY_BEFORE_FD_COMMAND");
    ERROR(syscall(SYS_flock,-1,0),EINVAL,"command-first"); mark("COMMAND_BEFORE_FD");
    ERROR(syscall(SYS_flock,-1,1),EBADF,"bad-fd");
    int path=open("/",O_PATH); check(path>=0,"path-open");
    ERROR(syscall(SYS_flock,path,8),EBADF,"path-unlock"); close(path); mark("VALID_COMMAND_BAD_FD"); done();

    begin("utimensat.raw-differential");
    struct timespec omit[2]={{.tv_sec=-1,.tv_nsec=UTIME_OMIT},{.tv_sec=-1,.tv_nsec=UTIME_OMIT}};
    check(syscall(SYS_utimensat,-1,BAD,omit,~0U)==0,"omit-bad-path");
    check(syscall(SYS_utimensat,-1,NULL,omit,~0U)==0,"omit-null-path"); mark("OMIT_BEFORE_PATH_FLAGS_FD");
    ERROR(syscall(SYS_utimensat,-1,NULL,BAD,~0U),EFAULT,"copy-first"); mark("COPY_BEFORE_FLAGS"); done();

    begin("fallocate.raw-differential");
    int pipes[2],pair[2]; check(pipe(pipes)==0,"pipe");
    ERROR(syscall(SYS_fallocate,pipes[1],0,0,1),ESPIPE,"fifo"); mark("FIFO_ESPIPE");
    ERROR(syscall(SYS_fallocate,pipes[0],0,0,1),EBADF,"read-pipe"); mark("ACCESS_BEFORE_TYPE");
    ERROR(syscall(SYS_fallocate,pipes[0],0x40000000,0,1),EOPNOTSUPP,"mode-before-access");
    ERROR(syscall(SYS_fallocate,pipes[1],2,0,1),EOPNOTSUPP,"punch-without-keep"); mark("MODE_BEFORE_ACCESS_TYPE");
    ERROR(syscall(SYS_fallocate,pipes[1],0,-1LL,1LL),EINVAL,"offset-first"); mark("GEOMETRY_BEFORE_TYPE");
    check(socketpair(AF_UNIX,SOCK_STREAM,0,pair)==0,"socketpair");
    ERROR(syscall(SYS_fallocate,pair[0],0,0,1),ENODEV,"socket"); mark("SOCKET_ENODEV");
    close(pair[0]);close(pair[1]);close(pipes[0]);close(pipes[1]);done();

    begin("readahead.raw-differential");
    int pidfd=syscall(SYS_pidfd_open,getpid(),0); check(pidfd>=0,"pidfd");
    ERROR(syscall(SYS_readahead,pidfd,0,1),EINVAL,"pidfd-readahead"); mark("PIDFD_EINVAL"); close(pidfd);
    ERROR(syscall(SYS_readahead,-1,-1LL,1),EBADF,"bad-fd-negative"); mark("FD_BEFORE_OFFSET");
    check(pipe(pipes)==0,"readahead-pipe");
    ERROR(syscall(SYS_readahead,pipes[0],0LL,1),EINVAL,"read-pipe");
    ERROR(syscall(SYS_readahead,pipes[0],-1LL,1),EINVAL,"read-pipe-negative"); mark("READ_PIPE_EINVAL");
    ERROR(syscall(SYS_readahead,pipes[1],0LL,1),EBADF,"write-pipe");
    ERROR(syscall(SYS_readahead,pipes[1],-1LL,1),EBADF,"write-pipe-negative"); mark("ACCESS_BEFORE_TYPE_OFFSET");
    close(pipes[0]); close(pipes[1]);
    path=open("/",O_PATH); check(path>=0,"readahead-path");
    ERROR(syscall(SYS_readahead,path,-1LL,1),EBADF,"path-negative"); close(path); mark("PATH_FD_EBADF"); done();
    begin("inotify_add_watch.raw-differential");
    unsigned conflict=IN_MASK_ADD|IN_MASK_CREATE|IN_MODIFY;
    ERROR(syscall(SYS_inotify_add_watch,-1,BAD,conflict),EBADF,"fd-before-conflict"); mark("FD_BEFORE_MASK_CONFLICT");
    ERROR(syscall(SYS_inotify_add_watch,-1,BAD,0),EINVAL,"empty-mask");
    ERROR(syscall(SYS_inotify_add_watch,-1,BAD,0x00800000),EINVAL,"unknown-mask"); mark("MASK_BITS_BEFORE_FD");
    int notify=syscall(SYS_inotify_init1,0); check(notify>=0,"inotify-create");
    ERROR(syscall(SYS_inotify_add_watch,notify,BAD,conflict),EINVAL,"conflict-before-path"); mark("MASK_CONFLICT_BEFORE_PATH"); close(notify); done();

    begin("signalfd4.raw-differential");
    uint64_t mask=0;
    ERROR(syscall(SYS_signalfd4,-2,BAD,8,~0U),EFAULT,"mask-before-flags"); mark("COPY_BEFORE_FLAGS_FD");
    ERROR(syscall(SYS_signalfd4,-2,BAD,0,~0U),EINVAL,"size-before-mask"); mark("SIZE_BEFORE_COPY");
    ERROR(syscall(SYS_signalfd4,-2,&mask,8,~0U),EINVAL,"flags-before-fd"); mark("FLAGS_BEFORE_FD");
    ERROR(syscall(SYS_signalfd4,-2,&mask,8,0),EBADF,"signalfd-bad-fd"); mark("VALID_MASK_FLAGS_BAD_FD"); done();

    begin("timerfd_settime.raw-differential");
    struct itimerspec timer={0};
    ERROR(syscall(SYS_timerfd_settime,-1,~0U,BAD,NULL),EFAULT,"timer-copy-first"); mark("COPY_BEFORE_FLAGS_FD");
    ERROR(syscall(SYS_timerfd_settime,-1,~0U,&timer,NULL),EINVAL,"timer-flags-first"); mark("FLAGS_BEFORE_FD");
    timer.it_value.tv_nsec=1000000000;
    ERROR(syscall(SYS_timerfd_settime,-1,0,&timer,NULL),EINVAL,"timer-invalid-spec"); mark("VALUE_BEFORE_FD");
    timer.it_value.tv_nsec=0;
    ERROR(syscall(SYS_timerfd_settime,-1,0,&timer,NULL),EBADF,"timer-bad-fd"); mark("VALID_VALUE_FLAGS_BAD_FD"); done();
    puts("THEKERNEL_FS_BOUNDARY_PASS"); return 0;
}
