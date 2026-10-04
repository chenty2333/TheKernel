/* Invoke rw ONLY on a disposable QEMU GPT image, never the Windows SSD. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
static unsigned char payload[128*1024], output[128*1024];
static void check(int ok,const char *what){if(!ok){perror(what);exit(1);}}
int main(int argc,char **argv){
 check(argc==2,"mode ro or rw");int fd=open("/dev/nvme0n1p1",O_RDWR);check(fd>=0,"open partition");
 int parent=open("/dev/nvme0n1",O_RDONLY);check(parent>=0,"open parent");
 unsigned int ro=99;unsigned long long bytes=0;
 check(ioctl(fd,0x125e,&ro)==0,"partition RO");check(ioctl(fd,0x80081272,&bytes)==0 && bytes==119ULL*1024*1024,"partition size");
 unsigned char magic[2],parent_magic[2];check(pread(fd,magic,2,1080)==2,"partition ext4 superblock");
 check(pread(parent,parent_magic,2,1048576+1080)==2 && !memcmp(magic,parent_magic,2) && magic[0]==0x53 && magic[1]==0xef,"partition offset maps exact parent bytes");close(parent);
 if(!strcmp(argv[1],"ro")){
  check(ro==1,"readonly default");errno=0;check(pwrite(fd,payload,512,0)==-1 && errno==EROFS,"partition rejects write");
  unsigned int zero=0;check(ioctl(fd,0x125d,&zero)==0,"software RO clear");check(pwrite(fd,payload,512,0)==-1,"hardware RO cannot be bypassed");
  unsigned int one=1;check(ioctl(fd,0x125d,&one)==0,"restore software RO");puts("NVME_GPT_RO_OK");
 }else{
  check(ro==0,"explicit write enable");unsigned int state=0xb1355eed;for(int i=0;i<(int)sizeof(payload);++i){state^=state<<13;state^=state>>17;state^=state<<5;payload[i]=state;}
  check(mkdir("/mnt",0755)==0 || errno==EEXIST,"mkdir");check(mkdir("/mnt/nvme",0755)==0 || errno==EEXIST,"mkdir child");
  check(mount("/dev/nvme0n1p1","/mnt/nvme","ext4",0,0)==0,"mount GPT ext4");
  int file=open("/mnt/nvme/partition-payload",O_CREAT|O_TRUNC|O_RDWR,0600);check(file>=0,"open payload");
  check(write(file,payload,sizeof(payload))==sizeof(payload),"write payload");check(fsync(file)==0,"flush payload");
  check(pread(file,output,sizeof(output),0)==sizeof(output) && !memcmp(payload,output,sizeof(payload)),"read compare");check(close(file)==0 && umount("/mnt/nvme")==0,"close unmount");
  check(fsync(fd)==0,"namespace Flush");puts("NVME_GPT_RW_OK");
 }
 close(fd);return 0;
}
