#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/signalfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/inotify.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#define BAD ((void *)(uintptr_t)1)
/* x86_64 kernel `sigset_t` is one 64-bit word; bit (signo-1) selects a signal. */
#define SIGBIT(signo) (1ULL << ((signo) - 1))
static const char *active;
static void begin(const char *name) { active=name; printf("THEKERNEL_ABI_CASE %s\n",name); }
static void check(int ok,const char *name) { if(!ok) { fprintf(stderr,"THEKERNEL_FS_BOUNDARY_FAIL %s %s errno=%d\n",active,name,errno); exit(1); } }
static void mark(const char *name) { printf("THEKERNEL_ABI_ASSERT %s %s pass\n",active,name); }
static void done(void) { printf("THEKERNEL_ABI_RESULT %s pass\n",active); }
#define ERROR(call,err,name) do { errno=0; long rc=(call); check(rc==-1 && errno==(err),name); } while(0)
static char probe[64];
static unsigned char scratch[4096];
static int poll_timeout(int fd,int ms) { struct pollfd p={.fd=fd,.events=POLLIN,.revents=0}; return poll(&p,1,ms); }
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

    /* A private watchable directory for the whole program.  It is created and
     * removed by this process only, so no shared guest state is disturbed. */
    snprintf(probe,sizeof probe,"/tmp/abi-fs-boundary-%d",(int)getpid());
    (void)rmdir(probe);

    /* inotify_init(2) is literally inotify_init1(2) with flags == 0:
     * `SYSCALL_DEFINE0(inotify_init) { return do_inotify_init(0); }`
     * (fs/notify/inotify/inotify_user.c:725-728).  do_inotify_init() passes
     * `O_RDONLY | flags` to anon_inode_getfd() (:718-720), and
     * __anon_inode_getfile() keeps only `O_ACCMODE | O_NONBLOCK` in f_flags
     * (fs/anon_inodes.c:163-164), so F_GETFL is exactly O_RDONLY and the new
     * descriptor has no FD_CLOEXEC. */
    begin("inotify_init.raw-differential");
    int plain=syscall(SYS_inotify_init); check(plain>=0,"init-fd");
    int plain_flags=fcntl(plain,F_GETFL);
    check(plain_flags>=0,"init-getfl");
    check((plain_flags&O_ACCMODE)==O_RDONLY,"init-readonly");
    check((plain_flags&O_NONBLOCK)==0,"init-blocking");
    check((plain_flags&O_CLOEXEC)==0,"init-status-cloexec");
    check((fcntl(plain,F_GETFD)&FD_CLOEXEC)==0,"init-fd-cloexec"); mark("INIT_FD_SHAPE");
    /* SYSCALL_DEFINE0 reads no arguments: garbage in the argument registers is
     * invisible to it. */
    int extra=syscall(SYS_inotify_init,BAD,BAD,BAD); check(extra>=0,"init-extra-args"); mark("INIT_IGNORES_EXTRA_ARGS");
    /* Every call builds an independent fsnotify group (:711-714), so a watch
     * added in one instance is unknown to another (inotify_idr_find() searches
     * the passed group's idr, :787-805). */
    check(mkdir(probe,0700)==0,"probe-mkdir");
    int second=syscall(SYS_inotify_init); check(second>=0,"init-second");
    check(second!=plain,"init-distinct-fd");
    int wd=syscall(SYS_inotify_add_watch,plain,probe,IN_MODIFY);
    check(wd>=1,"init-watch-id");
    ERROR(syscall(SYS_inotify_rm_watch,second,wd),EINVAL,"init-foreign-instance");
    check(syscall(SYS_inotify_rm_watch,plain,wd)==0,"init-own-instance"); mark("INIT_INSTANCE_INDEPENDENT");
    close(plain); close(second); close(extra); done();

    /* inotify_init1(2) rejects any bit outside IN_NONBLOCK|IN_CLOEXEC with
     * -EINVAL before it allocates a group (inotify_user.c:704-706), and the
     * accepted bits are reported exactly through F_GETFL/F_GETFD. */
    begin("inotify_init1.raw-differential");
    check(syscall(SYS_inotify_init1,0)>=0,"init1-zero");
    ERROR(syscall(SYS_inotify_init1,1),EINVAL,"init1-in-access");
    ERROR(syscall(SYS_inotify_init1,0x1000),EINVAL,"init1-unknown-bit");
    ERROR(syscall(SYS_inotify_init1,IN_NONBLOCK|IN_CLOEXEC|0x1),EINVAL,"init1-mixed"); mark("FLAG_MASK_EINVAL");
    int blocking=syscall(SYS_inotify_init1,0); check(blocking>=0,"init1-blocking");
    int block_flags=fcntl(blocking,F_GETFL);
    check((block_flags&O_ACCMODE)==O_RDONLY && (block_flags&O_NONBLOCK)==0 && (block_flags&O_CLOEXEC)==0,"init1-plain-status");
    check((fcntl(blocking,F_GETFD)&FD_CLOEXEC)==0,"init1-plain-cloexec");
    int cloexec=syscall(SYS_inotify_init1,IN_CLOEXEC); check(cloexec>=0,"init1-cloexec");
    int cloexec_flags=fcntl(cloexec,F_GETFL);
    check((cloexec_flags&O_ACCMODE)==O_RDONLY && (cloexec_flags&O_NONBLOCK)==0,"init1-cloexec-status");
    check((fcntl(cloexec,F_GETFD)&FD_CLOEXEC)!=0,"init1-cloexec-flag");
    int nonblocking=syscall(SYS_inotify_init1,IN_NONBLOCK); check(nonblocking>=0,"init1-nonblock");
    int nonblock_flags=fcntl(nonblocking,F_GETFL);
    check((nonblock_flags&O_NONBLOCK)!=0,"init1-nonblock-status");
    check((nonblock_flags&O_CLOEXEC)==0,"init1-nonblock-cloexec-status");
    check((fcntl(nonblocking,F_GETFD)&FD_CLOEXEC)==0,"init1-nonblock-cloexec-flag"); mark("FLAG_STATUS_REPORTED");
    /* An empty queue answers -EAGAIN before the record-size check
     * (`get_one_event()` returns NULL first, inotify_user.c:262-284), so even a
     * sub-header count is -EAGAIN rather than -EINVAL. */
    ERROR(read(nonblocking,scratch,1),EAGAIN,"init1-empty-short-read");
    ERROR(read(nonblocking,scratch,sizeof(struct inotify_event)),EAGAIN,"init1-empty-read"); mark("EMPTY_QUEUE_EAGAIN");
    int both=syscall(SYS_inotify_init1,IN_NONBLOCK|IN_CLOEXEC); check(both>=0,"init1-both");
    check((fcntl(both,F_GETFD)&FD_CLOEXEC)!=0,"init1-both-cloexec");
    check((fcntl(both,F_GETFL)&O_NONBLOCK)!=0,"init1-both-nonblock");
    int both_wd=syscall(SYS_inotify_add_watch,both,probe,IN_MODIFY); check(both_wd>=1,"init1-watch-id");
    check(syscall(SYS_inotify_rm_watch,both,both_wd)==0,"init1-watch-remove"); mark("NONBLOCK_CLOEXEC_COMBINED");
    close(blocking); close(cloexec); close(nonblocking); close(both); done();

    /* inotify_rm_watch(2) resolves the descriptor class before it looks at the
     * watch id (inotify_user.c:787-805), and a watch id is only meaningful
     * inside the instance that allocated it (inotify_idr_find() searches
     * `group->inotify_data.idr`, :414-430; ids start at 1 because
     * inotify_add_to_idr() calls idr_alloc_cyclic(idr, i_mark, 1, 0), :402). */
    begin("inotify_rm_watch.raw-differential");
    check(pipe(pipes)==0,"rm-pipe");
    int rmfd=syscall(SYS_inotify_init1,IN_NONBLOCK); check(rmfd>=0,"rm-instance");
    int rmwd=syscall(SYS_inotify_add_watch,rmfd,probe,IN_MODIFY); check(rmwd>=1,"rm-watch");
    ERROR(syscall(SYS_inotify_rm_watch,-1,rmwd),EBADF,"rm-bad-fd-first"); mark("FD_BEFORE_WD");
    int dead[2]; check(pipe(dead)==0,"rm-dead-pipe");
    close(dead[0]); close(dead[1]);
    ERROR(syscall(SYS_inotify_rm_watch,dead[1],rmwd),EBADF,"rm-closed-fd");
    ERROR(syscall(SYS_inotify_rm_watch,pipes[0],1),EINVAL,"rm-pipe-fd");
    ERROR(syscall(SYS_inotify_rm_watch,rmfd,0),EINVAL,"rm-zero-wd");
    ERROR(syscall(SYS_inotify_rm_watch,rmfd,rmwd+1),EINVAL,"rm-unknown-wd");
    ERROR(syscall(SYS_inotify_rm_watch,rmfd,-1),EINVAL,"rm-negative-wd");
    ERROR(syscall(SYS_inotify_rm_watch,rmfd,0x7fffffff),EINVAL,"rm-max-wd"); mark("UNKNOWN_WD_EINVAL");
    int foreign=syscall(SYS_inotify_init1,0); check(foreign>=0,"rm-foreign");
    ERROR(syscall(SYS_inotify_rm_watch,foreign,rmwd),EINVAL,"rm-foreign-wd"); mark("INSTANCE_LOCAL_WD");
    /* A successful removal queues exactly one IN_IGNORED record carrying the
     * removed id (inotify_ignored_and_remove_idr() -> queue_event(IN_IGNORED),
     * inotify_user.c, reached synchronously from fsnotify_destroy_mark(),
     * fs/notify/mark.c:662-669). */
    check(syscall(SYS_inotify_rm_watch,rmfd,rmwd)==0,"rm-success");
    check(poll_timeout(rmfd,1000)==1,"rm-ignored-ready");
    struct inotify_event event; memset(&event,0,sizeof event);
    check(read(rmfd,&event,sizeof event)==(ssize_t)sizeof event,"rm-ignored-read");
    check(event.wd==rmwd && event.mask==(uint32_t)IN_IGNORED && event.cookie==0 && event.len==0,"rm-ignored-record");
    ERROR(syscall(SYS_inotify_rm_watch,rmfd,rmwd),EINVAL,"rm-twice"); mark("REMOVE_QUEUES_IGNORED");
    close(rmfd); close(foreign); close(pipes[0]); close(pipes[1]); done();

    begin("signalfd4.raw-differential");
    uint64_t mask=0;
    ERROR(syscall(SYS_signalfd4,-2,BAD,8,~0U),EFAULT,"mask-before-flags"); mark("COPY_BEFORE_FLAGS_FD");
    ERROR(syscall(SYS_signalfd4,-2,BAD,0,~0U),EINVAL,"size-before-mask"); mark("SIZE_BEFORE_COPY");
    ERROR(syscall(SYS_signalfd4,-2,&mask,8,~0U),EINVAL,"flags-before-fd"); mark("FLAGS_BEFORE_FD");
    ERROR(syscall(SYS_signalfd4,-2,&mask,8,0),EBADF,"signalfd-bad-fd"); mark("VALID_MASK_FLAGS_BAD_FD"); done();

    /* signalfd(2) is signalfd4(2) with flags == 0, but with its own syscall
     * entry point: it validates sizemask, copies the set and only then enters
     * do_signalfd4() (fs/signalfd.c:299-321).  do_signalfd4() checks the flags
     * word (:257), strips SIGKILL/SIGSTOP and inverts the set (:260-261), then
     * either allocates an anon_inode via anon_inode_getfile_fmode() with
     * `O_RDWR | (flags & O_NONBLOCK)` (:266-274) or, for an existing
     * descriptor, requires signalfd_fops and rewrites only the mask
     * (:275-291). */
    begin("signalfd.raw-differential");
    uint64_t selected=SIGBIT(SIGUSR1), unselected=SIGBIT(SIGUSR2);
    ERROR(syscall(SYS_signalfd,-1,&selected,0),EINVAL,"sfd-zero-size");
    ERROR(syscall(SYS_signalfd,-1,&selected,4),EINVAL,"sfd-short-size");
    ERROR(syscall(SYS_signalfd,-1,&selected,16),EINVAL,"sfd-long-size"); mark("SIZEMASK_EINVAL");
    int sfpipe[2]; check(pipe(sfpipe)==0,"sfd-pipe");
    ERROR(syscall(SYS_signalfd,-1,BAD,8),EFAULT,"sfd-copy-before-create");
    ERROR(syscall(SYS_signalfd,sfpipe[0],BAD,8),EFAULT,"sfd-copy-before-fd"); mark("COPY_BEFORE_FD");
    int sfdead[2]; check(pipe(sfdead)==0,"sfd-dead-pipe");
    close(sfdead[0]); close(sfdead[1]);
    ERROR(syscall(SYS_signalfd,sfdead[0],&selected,8),EBADF,"sfd-closed-fd");
    ERROR(syscall(SYS_signalfd,sfpipe[0],&selected,8),EINVAL,"sfd-non-signalfd"); mark("FD_ADMISSION");
    int sfd=syscall(SYS_signalfd,-1,&selected,8); check(sfd>=0,"sfd-create");
    int sfd_flags=fcntl(sfd,F_GETFL);
    check((sfd_flags&O_ACCMODE)==O_RDWR,"sfd-rdwr");
    check((sfd_flags&O_NONBLOCK)==0,"sfd-blocking");
    check((fcntl(sfd,F_GETFD)&FD_CLOEXEC)==0,"sfd-no-cloexec");
    ERROR(read(sfd,scratch,127),EINVAL,"sfd-short-read"); mark("CREATE_STATUS_AND_RECORD_SIZE");
    /* The update form returns the same descriptor and leaves its creation
     * flags alone (:275-292). */
    check(syscall(SYS_signalfd,sfd,&unselected,8)==sfd,"sfd-update-identity");
    int updated_flags=fcntl(sfd,F_GETFL);
    check((updated_flags&O_ACCMODE)==O_RDWR && (updated_flags&O_NONBLOCK)==0,"sfd-update-status");
    check((fcntl(sfd,F_GETFD)&FD_CLOEXEC)==0,"sfd-update-cloexec"); mark("UPDATE_RETURNS_FD");
    /* The stored set selects which thread-pending, blocked signals this
     * descriptor may dequeue (signalfd_poll()/signalfd_dequeue() use
     * ctx->sigmask), and a delivered record is the 128-byte
     * `struct signalfd_siginfo` (:210-212, :58). */
    sigset_t blocked_saved; sigset_t block;
    sigemptyset(&block); sigaddset(&block,SIGUSR1);
    check(sigprocmask(SIG_BLOCK,&block,&blocked_saved)==0,"sfd-block-us1");
    check(raise(SIGUSR1)==0,"sfd-raise-us1");
    check(poll_timeout(sfd,0)==0,"sfd-unselected-idle"); mark("MASK_SELECTS_SIGNAL");
    check(syscall(SYS_signalfd,sfd,&selected,8)==sfd,"sfd-reselect");
    struct signalfd_siginfo info; memset(&info,0,sizeof info);
    check(read(sfd,&info,sizeof info)==(ssize_t)sizeof info,"sfd-record-read");
    check(info.ssi_signo==(uint32_t)SIGUSR1,"sfd-record-signo");
    check(info.ssi_code==SI_TKILL,"sfd-record-code");
    check(info.ssi_pid==(uint32_t)getpid(),"sfd-record-pid");
    check(info.ssi_uid==(uint32_t)getuid(),"sfd-record-uid"); mark("PENDING_RECORD_DELIVERED");
    check(sigprocmask(SIG_SETMASK,&blocked_saved,NULL)==0,"sfd-restore-mask");
    close(sfd); close(sfpipe[0]); close(sfpipe[1]); done();

    begin("timerfd_settime.raw-differential");
    struct itimerspec timer={0};
    ERROR(syscall(SYS_timerfd_settime,-1,~0U,BAD,NULL),EFAULT,"timer-copy-first"); mark("COPY_BEFORE_FLAGS_FD");
    ERROR(syscall(SYS_timerfd_settime,-1,~0U,&timer,NULL),EINVAL,"timer-flags-first"); mark("FLAGS_BEFORE_FD");
    timer.it_value.tv_nsec=1000000000;
    ERROR(syscall(SYS_timerfd_settime,-1,0,&timer,NULL),EINVAL,"timer-invalid-spec"); mark("VALUE_BEFORE_FD");
    timer.it_value.tv_nsec=0;
    ERROR(syscall(SYS_timerfd_settime,-1,0,&timer,NULL),EBADF,"timer-bad-fd"); mark("VALID_VALUE_FLAGS_BAD_FD"); done();
    check(rmdir(probe)==0,"probe-rmdir");
    puts("THEKERNEL_FS_BOUNDARY_PASS"); return 0;
}
