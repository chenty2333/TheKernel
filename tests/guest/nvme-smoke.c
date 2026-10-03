/* Destructive test: invoke ONLY with a disposable QEMU NVMe image. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#define SIZE (128*1024)
static unsigned char input[SIZE], output[SIZE];
static void require(int ok, const char *message) { if (!ok) { perror(message); exit(1); } }
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
