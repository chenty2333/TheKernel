#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <limits.h>
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
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
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
/* ---- metadata/lifecycle cases: utime(2), utimes(2), futimesat(2),
   fchmodat2(2), sync_file_range(2), cachestat(2), sync(2) ---- */
#define NR_UTIME 132
#define NR_UTIMES 235
#define NR_FUTIMESAT 261
#define NR_FCHMODAT2 452
#define NR_SYNC_FILE_RANGE 277
#define NR_CACHESTAT 451
#define NR_SYNC 162
#define OKC(call,name) check((call)==0,name)

struct fsb_utimbuf { long actime, modtime; };
struct fsb_tv { long tv_sec, tv_usec; };
struct fsb_csrange { uint64_t off, len; };
struct fsb_cachestat { uint64_t nr_cache, nr_dirty, nr_writeback, nr_evicted, nr_recently_evicted; };

static char meta_dir[64], meta_file[96], meta_link[96], meta_missing[112];

#define CH_TOUCH 0
#define CH_EXPLICIT 1
#define CH_CHMOD 2
#define CH_CACHESTAT 3
#define CH_CACHESTAT_FLAGS 4
#define CH_FUTIMESAT_FD 5
#define CH_FUTIMESAT_FD_NOW 6

/* Runs one operation in a child that has dropped to uid/gid 65534 with an
   empty supplementary group set, and returns 0 or the errno the child saw.
   The child itself never writes to stdout/stderr, so a failure line can never
   be interleaved with the parent's records. */
static int as_nobody(int op, const char *path, int fd, long arg) {
    int fds[2];
    if (pipe(fds) != 0) return -1;
    pid_t pid = fork();
    if (pid < 0) { close(fds[0]); close(fds[1]); return -1; }
    if (pid == 0) {
        close(fds[0]);
        int rc = 200;
        if (setgroups(0, NULL) == 0 && setresgid(65534, 65534, 65534) == 0 &&
            setresuid(65534, 65534, 65534) == 0) {
            errno = 0;
            switch (op) {
            case CH_TOUCH: {
                long r = syscall(NR_UTIME, path, NULL);
                rc = r == 0 ? 0 : errno;
                break;
            }
            case CH_EXPLICIT: {
                struct fsb_utimbuf t = { 5, 6 };
                long r = syscall(NR_UTIME, path, &t);
                rc = r == 0 ? 0 : errno;
                break;
            }
            case CH_CHMOD: {
                long r = syscall(NR_FCHMODAT2, AT_FDCWD, path, 0600, 0);
                rc = r == 0 ? 0 : errno;
                break;
            }
            case CH_CACHESTAT:
            case CH_CACHESTAT_FLAGS: {
                struct fsb_csrange range = { 0, 0 };
                struct fsb_cachestat out;
                long r = syscall(NR_CACHESTAT, fd, &range, &out, op == CH_CACHESTAT ? 0 : (int)arg);
                rc = r == 0 ? 0 : errno;
                break;
            }
            case CH_FUTIMESAT_FD:
            case CH_FUTIMESAT_FD_NOW: {
                struct fsb_tv t[2] = { { 5, 6 }, { 7, 8 } };
                long r = syscall(NR_FUTIMESAT, fd, NULL, op == CH_FUTIMESAT_FD ? t : NULL);
                rc = r == 0 ? 0 : errno;
                break;
            }
            default:
                break;
            }
        }
        unsigned char byte = (unsigned char)rc;
        ssize_t n = write(fds[1], &byte, 1);
        (void)n;
        _exit(0);
    }
    close(fds[1]);
    unsigned char byte = 0xff;
    ssize_t got = read(fds[0], &byte, 1);
    close(fds[0]);
    int status = 0;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || got != 1) return -1;
    return byte;
}

/* One 3-page scratch file, its symlink, and a directory to hold them. */
static void scratch(void) {
    strcpy(meta_dir, "/tmp/fsb-meta-XXXXXX");
    check(mkdtemp(meta_dir) != NULL, "scratch-mkdtemp");
    check(chmod(meta_dir, 0755) == 0, "scratch-mode");
    snprintf(meta_file, sizeof meta_file, "%s/file", meta_dir);
    snprintf(meta_link, sizeof meta_link, "%s/link", meta_dir);
    snprintf(meta_missing, sizeof meta_missing, "%s/missing", meta_dir);
    int fd = open(meta_file, O_CREAT | O_TRUNC | O_RDWR, 0644);
    check(fd >= 0, "scratch-create");
    char page[4096];
    memset(page, 'a', sizeof page);
    for (int i = 0; i < 3; i++) check(write(fd, page, sizeof page) == 4096, "scratch-write");
    check(close(fd) == 0, "scratch-close");
    check(symlink(meta_file, meta_link) == 0, "scratch-symlink");
}

static void cs_query(const char *name, int fd, uint64_t off, uint64_t len, unsigned flags,
                     struct fsb_cachestat *out) {
    struct fsb_csrange range = { off, len };
    memset(out, 0xee, sizeof *out);
    check(syscall(NR_CACHESTAT, fd, &range, out, flags) == 0, name);
}

static char probe[64];
static unsigned char sigbuf[4096];
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
    ERROR(read(nonblocking,sigbuf,1),EAGAIN,"init1-empty-short-read");
    ERROR(read(nonblocking,sigbuf,sizeof(struct inotify_event)),EAGAIN,"init1-empty-read"); mark("EMPTY_QUEUE_EAGAIN");
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
    scratch();

    /* ================= utime ================= */
    begin("utime.raw-differential");
    {
    /* fs/utimes.c:209-221: the two __kernel_old_time_t fields are fetched with
     * get_user() before do_utimes() looks the pathname up. */
    ERROR(syscall(NR_UTIME, meta_missing, BAD), EFAULT, "times-before-path");
    mark("TIMES_EFAULT_BEFORE_PATH");
    struct fsb_utimbuf tb = { 1234, 5678 };
    struct stat st;
    OKC(syscall(NR_UTIME, meta_file, &tb), "explicit");
    check(stat(meta_file, &st) == 0 && st.st_atim.tv_sec == 1234 && st.st_atim.tv_nsec == 0 &&
            st.st_mtim.tv_sec == 5678 && st.st_mtim.tv_nsec == 0,
        "explicit-seconds");
    mark("EXPLICIT_SECONDS_NANOS_ZERO");
    ERROR(syscall(NR_UTIME, meta_missing, &tb), ENOENT, "missing");
    mark("MISSING_PATH_ENOENT");
    long lo = (long)time(NULL);
    OKC(syscall(NR_UTIME, meta_file, NULL), "touch");
    long hi = (long)time(NULL);
    check(stat(meta_file, &st) == 0 && st.st_mtim.tv_sec >= lo && st.st_mtim.tv_sec <= hi &&
            st.st_atim.tv_sec >= lo,
        "touch-now");
    mark("NULL_TIMES_SETS_NOW");
    struct stat lst;
    check(lstat(meta_link, &lst) == 0, "lstat-link");
    long link_mtime = (long)lst.st_mtim.tv_sec;
    struct fsb_utimbuf tl = { 111, 222 };
    OKC(syscall(NR_UTIME, meta_link, &tl), "symlink");
    check(stat(meta_link, &st) == 0 && st.st_atim.tv_sec == 111 && st.st_mtim.tv_sec == 222,
        "symlink-target");
    check(lstat(meta_link, &lst) == 0 && (long)lst.st_mtim.tv_sec == link_mtime, "symlink-itself");
    mark("SYMLINK_FOLLOWS_TARGET");
    /* fs/attr.c:386-395 (ATTR_TOUCH) and fs/attr.c:218-222 (explicit times). */
    check(chmod(meta_file, 0666) == 0, "chmod-world-writable");
    check(as_nobody(CH_TOUCH, meta_file, -1, 0) == 0, "touch-may-write");
    mark("TOUCH_MAY_WRITE");
    check(as_nobody(CH_EXPLICIT, meta_file, -1, 0) == EPERM, "explicit-owner-only");
    mark("EXPLICIT_TIMES_OWNER_ONLY");
    check(chmod(meta_file, 0644) == 0, "chmod-owner-only");
    check(as_nobody(CH_TOUCH, meta_file, -1, 0) == EACCES, "touch-without-write");
    mark("TOUCH_WITHOUT_WRITE_EACCES");
    }
    done();

    /* ================= utimes ================= */
    begin("utimes.raw-differential");
    {
    struct stat st; long lo, hi;
    /* fs/utimes.c:168-194 with dfd == AT_FDCWD: copy, then both tv_usec range
     * checks, then do_utimes(). */
    ERROR(syscall(NR_UTIMES, meta_missing, BAD), EFAULT, "times-before-path");
    mark("TIMES_EFAULT_BEFORE_PATH");
    struct fsb_tv tv[2] = { { 0, 1000000 }, { 0, 0 } };
    ERROR(syscall(NR_UTIMES, meta_missing, tv), EINVAL, "usec-before-path");
    mark("USEC_RANGE_BEFORE_PATH");
    tv[0].tv_usec = -1;
    ERROR(syscall(NR_UTIMES, meta_missing, tv), EINVAL, "negative-usec");
    mark("NEGATIVE_USEC_EINVAL");
    /* UTIME_NOW/UTIME_OMIT are only valid for utimensat (fs/utimes.c:178-185). */
    tv[0].tv_usec = 0x3fffffff;
    ERROR(syscall(NR_UTIMES, meta_missing, tv), EINVAL, "special-values");
    mark("SPECIAL_VALUES_REJECTED");
    /* Microsecond precision is preserved as nsec = 1000 * usec. */
    tv[0].tv_sec = 100; tv[0].tv_usec = 123456;
    tv[1].tv_sec = 200; tv[1].tv_usec = 7;
    OKC(syscall(NR_UTIMES, meta_file, tv), "microseconds");
    check(stat(meta_file, &st) == 0 && st.st_atim.tv_sec == 100 && st.st_atim.tv_nsec == 123456000 &&
            st.st_mtim.tv_sec == 200 && st.st_mtim.tv_nsec == 7000,
        "microsecond-value");
    mark("MICROSECOND_PRECISION");
    lo = (long)time(NULL);
    OKC(syscall(NR_UTIMES, meta_file, NULL), "null-times");
    hi = (long)time(NULL);
    check(stat(meta_file, &st) == 0 && st.st_mtim.tv_sec >= lo && st.st_mtim.tv_sec <= hi, "null-now");
    mark("NULL_TIMES_SETS_NOW");
    ERROR(syscall(NR_UTIMES, meta_missing, NULL), ENOENT, "missing-null");
    mark("NULL_TIMES_MISSING_PATH_ENOENT");
    /* A fault in the second timeval is EFAULT and no time is applied. */
    {
        long ps = sysconf(_SC_PAGESIZE);
        char *p = mmap(NULL, (size_t)ps * 2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS,
                       -1, 0);
        check(p != MAP_FAILED, "fault-mmap");
        struct fsb_tv *pair = (struct fsb_tv *)(p + ps - sizeof(struct fsb_tv));
        pair[0].tv_sec = 555; pair[0].tv_usec = 0;
        check(munmap(p + ps, (size_t)ps) == 0, "fault-munmap");
        struct stat before;
        check(stat(meta_file, &before) == 0, "fault-stat");
        ERROR(syscall(NR_UTIMES, meta_file, pair), EFAULT, "partial-fault");
        check(stat(meta_file, &st) == 0 && st.st_mtim.tv_sec == before.st_mtim.tv_sec, "fault-unchanged");
        mark("PARTIAL_FAULT_LEAVES_TIMES");
        munmap(p, (size_t)ps);
    }
    }
    done();

    /* ================= futimesat ================= */
    begin("futimesat.raw-differential");
    {
    struct fsb_tv tv[2]; struct stat st; long lo, hi;
    ERROR(syscall(NR_FUTIMESAT, -1, NULL, BAD), EFAULT, "times-before-fd");
    mark("TIMES_EFAULT_BEFORE_FD");
    tv[0].tv_sec = 0; tv[0].tv_usec = 1000000; tv[1].tv_sec = 0; tv[1].tv_usec = 0;
    ERROR(syscall(NR_FUTIMESAT, -1, NULL, tv), EINVAL, "usec-before-fd");
    mark("USEC_RANGE_BEFORE_FD");
    tv[0].tv_usec = 0;
    ERROR(syscall(NR_FUTIMESAT, -1, NULL, tv), EBADF, "null-path-bad-fd");
    mark("NULL_PATH_BAD_FD_EBADF");
    ERROR(syscall(NR_FUTIMESAT, AT_FDCWD, NULL, tv), EFAULT, "at-fdcwd-null-path");
    mark("AT_FDCWD_NULL_PATH_EFAULT");
    /* fs/utimes.c:108-117: the descriptor form takes the description with
     * CLASS(fd, f), which rejects FMODE_PATH (fs/file.c:1196). */
    int path_fd = open(meta_file, O_PATH);
    check(path_fd >= 0, "opath-open");
    ERROR(syscall(NR_FUTIMESAT, path_fd, NULL, tv), EBADF, "null-path-opath");
    mark("NULL_PATH_OPATH_FD_EBADF");
    close(path_fd);
    /* The descriptor form targets the open description itself. */
    int fd = open(meta_file, O_RDWR);
    check(fd >= 0, "fd-open");
    tv[0].tv_sec = 77; tv[0].tv_usec = 88000;
    tv[1].tv_sec = 99; tv[1].tv_usec = 1000;
    OKC(syscall(NR_FUTIMESAT, fd, NULL, tv), "fd-form");
    check(fstat(fd, &st) == 0 && st.st_atim.tv_sec == 77 && st.st_atim.tv_nsec == 88000 * 1000 &&
            st.st_mtim.tv_sec == 99 && st.st_mtim.tv_nsec == 1000 * 1000,
        "fd-form-value");
    mark("FD_FORM_MICROSECOND_PRECISION");
    lo = (long)time(NULL);
    OKC(syscall(NR_FUTIMESAT, fd, NULL, NULL), "fd-form-now");
    hi = (long)time(NULL);
    check(fstat(fd, &st) == 0 && st.st_mtim.tv_sec >= lo && st.st_mtim.tv_sec <= hi, "fd-now-value");
    mark("FD_FORM_NULL_TIMES_NOW");
    /* A pipe endpoint is a pseudo inode: the same descriptor form applies the
     * explicit times, and a non-owner without CAP_FOWNER is refused. */
    int pfd[2];
    check(pipe(pfd) == 0, "pipe");
    tv[0].tv_sec = 7; tv[0].tv_usec = 8;
    OKC(syscall(NR_FUTIMESAT, pfd[0], NULL, tv), "pseudo-inode");
    check(as_nobody(CH_FUTIMESAT_FD, NULL, pfd[0], 0) == EPERM, "pseudo-inode-nonowner");
    check(as_nobody(CH_FUTIMESAT_FD_NOW, NULL, pfd[0], 0) == EACCES, "pseudo-inode-nonowner-now");
    mark("PSEUDO_INODE_DESCRIPTOR_FORM");
    /* A pathname is resolved relative to a real dirfd; an empty pathname is
     * ENOENT because futimesat has no AT_EMPTY_PATH flag. */
    int dirfd = open(meta_dir, O_RDONLY | O_DIRECTORY);
    check(dirfd >= 0, "dirfd-open");
    tv[0].tv_sec = 11; tv[0].tv_usec = 1;
    tv[1].tv_sec = 22; tv[1].tv_usec = 2;
    OKC(syscall(NR_FUTIMESAT, dirfd, "file", tv), "relative-path");
    check(fstat(fd, &st) == 0 && st.st_atim.tv_sec == 11 && st.st_mtim.tv_sec == 22, "relative-value");
    mark("RELATIVE_PATH_TO_FD");
    ERROR(syscall(NR_FUTIMESAT, dirfd, "missing", tv), ENOENT, "relative-missing");
    mark("RELATIVE_MISSING_ENOENT");
    ERROR(syscall(NR_FUTIMESAT, 9999, "file", tv), EBADF, "bad-dirfd");
    mark("BAD_DIRFD_EBADF");
    ERROR(syscall(NR_FUTIMESAT, dirfd, "", tv), ENOENT, "empty-path");
    mark("EMPTY_PATH_ENOENT");
    /* The descriptor form keeps working after the name is gone, because it
     * targets the description rather than a pathname (fs/utimes.c:116). */
    {
        char tmp[112];
        snprintf(tmp, sizeof tmp, "%s/unlinked", meta_dir);
        int u = open(tmp, O_CREAT | O_RDWR, 0644);
        check(u >= 0, "unlinked-open");
        check(unlink(tmp) == 0, "unlinked-unlink");
        struct fsb_tv t2[2] = { { 31, 32 }, { 33, 34 } };
        OKC(syscall(NR_FUTIMESAT, u, NULL, t2), "unlinked-fd");
        check(fstat(u, &st) == 0 && st.st_atim.tv_sec == 31 && st.st_mtim.tv_sec == 33,
            "unlinked-fd-value");
        mark("UNLINKED_FD_FORM_OK");
        close(u);
    }
    }
    done();

    /* ================= fchmodat2 ================= */
    begin("fchmodat2.raw-differential");
    {
    struct stat st; int path_fd;
    /* fs/open.c:667-702 rejects unknown flag bits before the pathname is copied. */
    ERROR(syscall(NR_FCHMODAT2, AT_FDCWD, BAD, 0640, 0x8000U), EINVAL, "flags-before-path");
    mark("FLAGS_BEFORE_PATH");
    ERROR(syscall(NR_FCHMODAT2, -1, NULL, 0640, ~0U), EINVAL, "flags-mask-before-null");
    mark("FLAGS_MASK_BEFORE_NULL_PATH");
    /* fs/namei.c:233-238 maps an empty pathname to LOOKUP_EMPTY only with
     * AT_EMPTY_PATH, and path_init uses fd_raw for that lookup. */
    ERROR(syscall(NR_FCHMODAT2, AT_FDCWD, "", 0640, 0), ENOENT, "empty-without-flag");
    mark("EMPTY_PATH_WITHOUT_FLAG_ENOENT");
    ERROR(syscall(NR_FCHMODAT2, -1, "", 0640, AT_EMPTY_PATH), EBADF, "empty-bad-fd");
    mark("EMPTY_PATH_BAD_FD_EBADF");
    ERROR(syscall(NR_FCHMODAT2, AT_FDCWD, NULL, 0640, AT_EMPTY_PATH), EFAULT, "null-path");
    mark("NULL_PATH_EFAULT");
    path_fd = open(meta_file, O_PATH);
    check(path_fd >= 0, "opath-open");
    OKC(syscall(NR_FCHMODAT2, path_fd, "", 0645, AT_EMPTY_PATH), "empty-path-opath");
    check(fstat(path_fd, &st) == 0 && (st.st_mode & 07777) == 0645, "empty-path-opath-mode");
    mark("EMPTY_PATH_OPATH_FD_COMMITS_MODE");
    close(path_fd);
    OKC(syscall(NR_FCHMODAT2, AT_FDCWD, meta_file, 0642, AT_EMPTY_PATH), "named-with-flag");
    check(stat(meta_file, &st) == 0 && (st.st_mode & 07777) == 0642, "named-with-flag-mode");
    mark("NAMED_PATH_WITH_EMPTY_FLAG_COMMITS_MODE");
    /* The low 12 mode bits are published verbatim. */
    OKC(syscall(NR_FCHMODAT2, AT_FDCWD, meta_file, 04755, 0), "setuid-bit");
    check(stat(meta_file, &st) == 0 && (st.st_mode & 07777) == 04755, "setuid-bit-mode");
    mark("MODE_BITS_VERBATIM");
    /* fs/attr.c:442-458 refuses a mode change on a symlink. */
    ERROR(syscall(NR_FCHMODAT2, AT_FDCWD, meta_link, 0600, AT_SYMLINK_NOFOLLOW), EOPNOTSUPP,
         "symlink-nofollow");
    mark("SYMLINK_NOFOLLOW_EOPNOTSUPP");
    OKC(syscall(NR_FCHMODAT2, AT_FDCWD, meta_file, 0604, AT_SYMLINK_NOFOLLOW), "regular-nofollow");
    check(stat(meta_file, &st) == 0 && (st.st_mode & 07777) == 0604, "regular-nofollow-mode");
    mark("NOFOLLOW_REGULAR_COMMITS");
    /* fs/attr.c:202-206 requires the owner or CAP_FOWNER. */
    check(as_nobody(CH_CHMOD, meta_file, -1, 0) == EPERM, "chmod-nonowner");
    mark("NONOWNER_EPERM");
    /* Directory mode bits, including the sticky bit, are published verbatim. */
    OKC(syscall(NR_FCHMODAT2, AT_FDCWD, meta_dir, 01777, 0), "directory-sticky");
    check(stat(meta_dir, &st) == 0 && (st.st_mode & 07777) == 01777, "directory-sticky-mode");
    mark("DIRECTORY_STICKY_BITS");
    check(chmod(meta_dir, 0755) == 0, "directory-mode-restore");
    }
    done();

    /* ================= sync_file_range ================= */
    begin("sync_file_range.raw-differential");
    {
    int pfd[2]; int fd; int path_fd;
    /* fs/sync.c:224-296: descriptor, then flags, then the offset/nbytes
     * arithmetic, then the inode type. */
    ERROR(syscall(NR_SYNC_FILE_RANGE, -1, 0, 0, 8), EBADF, "fd-before-flags");
    mark("FD_BEFORE_FLAGS");
    path_fd = open(meta_file, O_PATH);
    check(path_fd >= 0, "opath-open");
    ERROR(syscall(NR_SYNC_FILE_RANGE, path_fd, 0, 0, 0), EBADF, "opath-fd");
    mark("OPATH_FD_EBADF");
    close(path_fd);
    check(pipe(pfd) == 0, "pipe");
    ERROR(syscall(NR_SYNC_FILE_RANGE, pfd[0], 0, 0, 8), EINVAL, "flags-before-type");
    mark("FLAGS_BEFORE_TYPE");
    ERROR(syscall(NR_SYNC_FILE_RANGE, pfd[0], -1LL, 0, 0), EINVAL, "offset-sign-before-type");
    mark("OFFSET_SIGN_BEFORE_TYPE");
    ERROR(syscall(NR_SYNC_FILE_RANGE, pfd[0], 0, -1LL, 0), EINVAL, "length-sign-before-type");
    mark("LENGTH_SIGN_BEFORE_TYPE");
    ERROR(syscall(NR_SYNC_FILE_RANGE, pfd[0], LLONG_MAX, 1, 0), EINVAL, "wrapping-end");
    mark("WRAPPING_END_EINVAL");
    ERROR(syscall(NR_SYNC_FILE_RANGE, pfd[0], 0, 0, 0), ESPIPE, "fifo");
    mark("FIFO_ESPIPE");
    int sp[2];
    check(socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0, "socketpair");
    ERROR(syscall(NR_SYNC_FILE_RANGE, sp[0], 0, 0, 0), ESPIPE, "socket");
    mark("SOCKET_ESPIPE");
    fd = open(meta_file, O_RDWR);
    check(fd >= 0, "fd-open");
    OKC(syscall(NR_SYNC_FILE_RANGE, fd, 0, 0, 0), "regular");
    mark("REGULAR_RANGE_OK");
    OKC(syscall(NR_SYNC_FILE_RANGE, fd, 0, 0, 1 | 2 | 4), "all-flags");
    mark("ALL_FLAGS_ACCEPTED");
    OKC(syscall(NR_SYNC_FILE_RANGE, fd, LLONG_MAX, 0, 0), "eof-from-llong-max");
    mark("EOF_FROM_LLONG_MAX_OK");
    int ro = open(meta_file, O_RDONLY);
    check(ro >= 0, "readonly-open");
    OKC(syscall(NR_SYNC_FILE_RANGE, ro, 0, 4096, 3), "readonly");
    mark("READONLY_DESCRIPTION_OK");
    int dfd = open(meta_dir, O_RDONLY | O_DIRECTORY);
    check(dfd >= 0, "dirfd-open");
    OKC(syscall(NR_SYNC_FILE_RANGE, dfd, 0, 0, 0), "directory");
    mark("DIRECTORY_OK");
    {
        char page[4096], back[4096];
        memset(page, 'q', sizeof page);
        check(pwrite(fd, page, sizeof page, 4096) == 4096, "range-write");
        OKC(syscall(NR_SYNC_FILE_RANGE, fd, 4096, 4096, 3), "range-hint");
        memset(back, 0, sizeof back);
        check(pread(fd, back, sizeof back, 4096) == 4096 && memcmp(page, back, sizeof page) == 0,
              "range-readback");
        mark("HINT_PRESERVES_CONTENT");
    }
    }
    done();

    /* ================= cachestat ================= */
    begin("cachestat.raw-differential");
    {
    int path_fd; int fd; int dfd; int ro;
    {
        struct fsb_cachestat out, whole, onepage;
        /* mm/filemap.c:4752-4787: descriptor, range copyin, hugetlb, admission,
         * flags, then the counters and copyout. */
        ERROR(syscall(NR_CACHESTAT, -1, BAD, BAD, 1), EBADF, "fd-before-range");
        mark("FD_BEFORE_RANGE_COPY");
        path_fd = open(meta_file, O_PATH);
        check(path_fd >= 0, "opath-open");
        ERROR(syscall(NR_CACHESTAT, path_fd, BAD, BAD, 1), EBADF, "opath-fd");
        mark("OPATH_FD_EBADF");
        close(path_fd);
        fd = open(meta_file, O_RDWR);
        check(fd >= 0, "fd-open");
        ERROR(syscall(NR_CACHESTAT, fd, BAD, &out, 1), EFAULT, "range-copy-before-flags");
        mark("RANGE_COPY_BEFORE_FLAGS");
        ERROR(syscall(NR_CACHESTAT, fd, &(struct fsb_csrange){ 0, 0 }, &out, 1), EINVAL, "flags-einval");
        mark("FLAGS_EINVAL");
        ERROR(syscall(NR_CACHESTAT, fd, &(struct fsb_csrange){ 0, 0 }, BAD, 1), EINVAL,
             "flags-before-copyout");
        mark("FLAGS_BEFORE_COPYOUT");
        ERROR(syscall(NR_CACHESTAT, fd, &(struct fsb_csrange){ 0, 0 }, BAD, 0), EFAULT, "copyout");
        mark("COPYOUT_EFAULT");
        /* mm/filemap.c:4700-4730 admission precedes the flags word. */
        check(chmod(meta_file, 0644) == 0, "chmod-owner-only");
        ro = open(meta_file, O_RDONLY);
        check(ro >= 0, "readonly-open");
        check(as_nobody(CH_CACHESTAT, NULL, ro, 0) == EPERM, "admission-nonowner");
        check(as_nobody(CH_CACHESTAT_FLAGS, NULL, ro, 1) == EPERM, "admission-before-flags");
        mark("ADMISSION_BEFORE_FLAGS");
        close(ro);
        check(chmod(meta_file, 0666) == 0, "chmod-world-writable");
        /* Objects without a vfsmount are authorized through their pseudo inode. */
        int qp[2];
        check(pipe(qp) == 0, "pipe");
        cs_query("pipe-read-end", qp[0], 0, 0, 0, &out);
        check(out.nr_cache == 0 && out.nr_dirty == 0 && out.nr_writeback == 0 && out.nr_evicted == 0 &&
                out.nr_recently_evicted == 0,
            "pipe-counters");
        mark("ANONYMOUS_PIPE_READ_END_OK");
        int qs[2];
        check(socketpair(AF_UNIX, SOCK_STREAM, 0, qs) == 0, "socketpair");
        cs_query("socket", qs[0], 0, 0, 0, &out);
        check(out.nr_cache == 0 && out.nr_dirty == 0, "socket-counters");
        mark("SOCKET_QUERY_OK");
        dfd = open(meta_dir, O_RDONLY | O_DIRECTORY);
        check(dfd >= 0, "dirfd-open");
        cs_query("directory", dfd, 0, 0, 0, &out);
        mark("DIRECTORY_QUERY_OK");
        /* An empty inclusive page interval reports no page in any field. */
        cs_query("wrapping", fd, UINT64_MAX, 2, 0, &out);
        check(out.nr_cache == 0 && out.nr_dirty == 0 && out.nr_writeback == 0 && out.nr_evicted == 0 &&
                out.nr_recently_evicted == 0,
            "wrapping-counters");
        cs_query("beyond-eof", fd, 3 * 4096, 0, 0, &out);
        check(out.nr_cache == 0 && out.nr_dirty == 0 && out.nr_writeback == 0 && out.nr_evicted == 0 &&
                out.nr_recently_evicted == 0,
            "eof-counters");
        mark("EMPTY_RANGE_COUNTERS_ZERO");
        /* Counter invariants: resident accounting is private to the backing
         * filesystem, so only relations that hold in every Linux cache state
         * are asserted.  Dirty and writeback pages are resident; every page
         * counted lies inside the queried interval; nothing was evicted. */
        {
            char buf[4096];
            for (int i = 0; i < 3; i++) check(pread(fd, buf, sizeof buf, (off_t)i * 4096) == 4096, "pread");
        }
        cs_query("whole", fd, 0, 0, 0, &whole);
        cs_query("first-page", fd, 0, 4096, 0, &onepage);
        check(whole.nr_dirty <= whole.nr_cache && whole.nr_writeback <= whole.nr_cache &&
                whole.nr_cache <= 3 && whole.nr_evicted == 0 && whole.nr_recently_evicted == 0 &&
                onepage.nr_cache <= whole.nr_cache,
            "counter-invariants");
        mark("COUNTER_INVARIANTS");
        /* The output is exactly 40 bytes copied to the supplied address. */
        {
            char raw[48], out_raw[48];
            struct fsb_cachestat zero;
            memset(raw, 0x5a, sizeof raw);
            struct fsb_csrange range = { UINT64_MAX, 2 };
            memcpy(raw + 1, &range, sizeof range);
            memset(out_raw, 0x5a, sizeof out_raw);
            memset(&zero, 0xee, sizeof zero);
            errno = 0;
            check(syscall(NR_CACHESTAT, fd, raw + 1, out_raw + 1, 0) == 0, "unaligned");
            check(out_raw[0] == 0x5a && out_raw[41] == 0x5a && out_raw[47] == 0x5a, "copyout-guard");
            memcpy(&zero, out_raw + 1, sizeof zero);
            check(zero.nr_cache == 0 && zero.nr_dirty == 0 && zero.nr_writeback == 0 &&
                      zero.nr_evicted == 0 && zero.nr_recently_evicted == 0,
                  "unaligned-counters");
            mark("UNALIGNED_AND_40_BYTE_COPYOUT");
        }
        /* mm/filemap.c:4768-4769 classifies hugetlbfs right after copyin. */
        {
            char hdir[112], hfile[128];
            snprintf(hdir, sizeof hdir, "%s/huge", meta_dir);
            check(mkdir(hdir, 0755) == 0, "huge-mkdir");
            errno = 0;
            if (mount("none", hdir, "hugetlbfs", 0, NULL) == 0) {
                snprintf(hfile, sizeof hfile, "%s/f", hdir);
                int hf = open(hfile, O_CREAT | O_RDWR, 0644);
                check(hf >= 0, "huge-open");
                ERROR(syscall(NR_CACHESTAT, hf, &(struct fsb_csrange){ 0, 0 }, &out, 0), EOPNOTSUPP,
                     "hugetlbfs");
                mark("HUGETLBFS_EOPNOTSUPP");
                close(hf);
                umount2(hdir, MNT_DETACH);
            }
        }
        close(fd);
    }
    }
    done();

    /* ================= sync ================= */
    begin("sync.raw-differential");
    {
    /* fs/sync.c:97-113: SYSCALL_DEFINE0(sync) reads no argument and always
     * reports success. */
    OKC(syscall(NR_SYNC), "sync");
    OKC(syscall(NR_SYNC, 0xdeadbeefUL, 0xdeadbeefUL, 0xdeadbeefUL, 0xdeadbeefUL, 0xdeadbeefUL,
                0xdeadbeefUL),
        "garbage-arguments");
    mark("IGNORES_ARGUMENTS");
    {
        char page[4096], back[4096];
        memset(page, 'z', sizeof page);
        int wf = open(meta_file, O_RDWR);
        check(wf >= 0, "sync-open");
        check(pwrite(wf, page, sizeof page, 8192) == 4096, "sync-write");
        OKC(syscall(NR_SYNC), "sync-flush");
        int rf = open(meta_file, O_RDONLY);
        check(rf >= 0, "sync-reopen");
        memset(back, 0, sizeof back);
        check(pread(rf, back, sizeof back, 8192) == 4096 && memcmp(page, back, sizeof page) == 0,
              "sync-readback");
        mark("DATA_VISIBLE_AFTER_SYNC");
        close(rf);
        close(wf);
    }
    }
    done();
    check(rmdir(probe)==0,"probe-rmdir");
    puts("THEKERNEL_FS_BOUNDARY_PASS"); return 0;
}
