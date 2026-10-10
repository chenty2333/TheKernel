/* Destructive test: invoke ONLY with a disposable QEMU NVMe image. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <unistd.h>
#define SIZE (128*1024)
#define BATCH_WORKERS 4
#define BATCH_ROUNDS 4
#define BATCH_SIZE (16*1024)
#define BATCH_START (56*1024*1024LL)
static unsigned char input[SIZE], output[SIZE];
static void require(int ok, const char *message) { if (!ok) { perror(message); exit(1); } }

struct batch_worker {
    int fd;
    int worker;
    atomic_int *start;
    int error;
    int phase;
};

static void fill_batch_payload(unsigned char *buffer, int worker, int round) {
    unsigned int random = 0x6d2b79f5u ^ (unsigned int)worker * 0x9e3779b9u ^
                          (unsigned int)round * 0x85ebca6bu;
    for (int i = 0; i < BATCH_SIZE; ++i) {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        buffer[i] = (unsigned char)random;
    }
}

static int transfer_at(int fd, unsigned char *buffer, size_t length,
                       off_t offset, int write) {
    size_t transferred = 0;
    while (transferred < length) {
        ssize_t result = write
            ? pwrite(fd, buffer + transferred, length - transferred,
                     offset + (off_t)transferred)
            : pread(fd, buffer + transferred, length - transferred,
                    offset + (off_t)transferred);
        if (result < 0 && errno == EINTR) continue;
        if (result <= 0) {
            if (result == 0) errno = EIO;
            return -1;
        }
        transferred += (size_t)result;
    }
    return 0;
}

static void *run_batch_worker(void *opaque) {
    struct batch_worker *worker = opaque;
    unsigned char written[BATCH_SIZE], readback[BATCH_SIZE];
    while (!atomic_load_explicit(worker->start, memory_order_acquire)) sched_yield();

    off_t offset = BATCH_START + (off_t)worker->worker * BATCH_SIZE;
    for (int round = 0; round < BATCH_ROUNDS; ++round) {
        fill_batch_payload(written, worker->worker, round);
        if (transfer_at(worker->fd, written, sizeof(written), offset, 1) < 0) {
            worker->error = errno ? errno : EIO;
            worker->phase = 1;
            return NULL;
        }
        if (transfer_at(worker->fd, readback, sizeof(readback), offset, 0) < 0) {
            worker->error = errno ? errno : EIO;
            worker->phase = 2;
            return NULL;
        }
        if (memcmp(written, readback, sizeof(written))) {
            worker->error = EIO;
            worker->phase = 3;
            return NULL;
        }
    }
    return NULL;
}

static int run_concurrent_batch(int fd) {
    pthread_t threads[BATCH_WORKERS];
    struct batch_worker workers[BATCH_WORKERS] = {0};
    atomic_int start;
    atomic_init(&start, 0);
    int started = 0, create_error = 0, join_error = 0;

    for (int i = 0; i < BATCH_WORKERS; ++i) {
        workers[i].fd = fd;
        workers[i].worker = i;
        workers[i].start = &start;
        int error = pthread_create(&threads[i], NULL, run_batch_worker, &workers[i]);
        if (error) {
            create_error = error;
            break;
        }
        ++started;
    }

    // Release all successfully created workers together, even if creation of
    // a later worker failed. Every started thread is joined before returning.
    atomic_store_explicit(&start, 1, memory_order_release);
    for (int i = 0; i < started; ++i) {
        int error = pthread_join(threads[i], NULL);
        if (error && !join_error) join_error = error;
    }
    if (create_error || join_error) {
        errno = create_error ? create_error : join_error;
        return -1;
    }
    for (int i = 0; i < BATCH_WORKERS; ++i) {
        if (workers[i].error) {
            errno = workers[i].error;
            fprintf(stderr, "NVMe batch worker %d phase %d failed\n",
                    workers[i].worker, workers[i].phase);
            return -1;
        }
    }

    if (fsync(fd) < 0) return -1;
    unsigned char expected[BATCH_SIZE], actual[BATCH_SIZE];
    for (int i = 0; i < BATCH_WORKERS; ++i) {
        fill_batch_payload(expected, i, BATCH_ROUNDS - 1);
        off_t offset = BATCH_START + (off_t)i * BATCH_SIZE;
        if (transfer_at(fd, actual, sizeof(actual), offset, 0) < 0) return -1;
        if (memcmp(expected, actual, sizeof(expected))) {
            errno = EIO;
            return -1;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    require(argc == 2, "mode ro/raw/fs");
    unsigned int random = 0xb1355eed;
    for (int i=0; i<SIZE; ++i) { random ^= random << 13; random ^= random >> 17; random ^= random << 5; input[i] = random; }
    int fd=open("/dev/nvme0n1",O_RDWR); require(fd>=0,"open nvme");
    unsigned int ro=99; require(ioctl(fd,0x125e,&ro)==0,"BLKROGET");
    if (!strcmp(argv[1],"ro")) {
        require(ro==1,"default read only");
        errno=0; require(pwrite(fd,input,512,60*1024*1024)==-1 && errno==EROFS,"read-only write rejected");
        require(pread(fd,output,SIZE,0)==SIZE,"read-only read");
        puts("NVME_RO_OK");
    } else {
        require(ro==0,"writes explicitly enabled");
        if (!strcmp(argv[1],"raw")) {
            require(pwrite(fd,input,SIZE,60*1024*1024)==SIZE,"raw write PRP list");
            require(fsync(fd)==0,"flush");
            require(pread(fd,output,SIZE,60*1024*1024)==SIZE,"raw read PRP list");
            require(!memcmp(input,output,SIZE),"raw content compare");
            puts("NVME_RAW_OK");
            require(run_concurrent_batch(fd)==0,"concurrent disjoint-region batch");
            puts("NVME_BATCH_OK");
        } else {
            require(mkdir("/mnt",0755)==0 || errno==EEXIST,"mkdir parent");
            require(mkdir("/mnt/nvme",0755)==0 || errno==EEXIST,"mkdir");
            require(mount("/dev/nvme0n1","/mnt/nvme","ext4",0,NULL)==0,"mount ext4");
            int file=open("/mnt/nvme/payload",O_CREAT|O_TRUNC|O_RDWR,0600); require(file>=0,"open file");
            require(write(file,input,SIZE)==SIZE,"write file"); require(fsync(file)==0,"file flush");
            require(pread(file,output,SIZE,0)==SIZE && !memcmp(input,output,SIZE),"file readback");
            require(close(file)==0,"close"); require(umount("/mnt/nvme")==0,"unmount");
            puts("NVME_EXT4_OK");
        }
    }
    close(fd); return 0;
}
