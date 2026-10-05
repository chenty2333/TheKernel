#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/swap.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096
#define PAGES 16
struct observed {unsigned long start,end;unsigned long long size,rss,swap;};
static void need(int good,const char *name) {if(!good){fprintf(stderr,"proc-smaps: %s errno=%d (%s)\n",name,errno,strerror(errno));exit(1);}}
static struct observed observe(const void *mapping) {
    FILE *file=fopen("/proc/self/smaps","r");need(file!=NULL,"smaps open");
    char line[512];struct observed result={0};unsigned fields=0;int selected=0;
    while(fgets(line,sizeof(line),file)) {
        unsigned long start,end;char permissions[5];
        if(sscanf(line,"%lx-%lx %4s",&start,&end,permissions)==3) {
            if(selected)break;
            selected=(uintptr_t)mapping>=start && (uintptr_t)mapping<end;
            if(selected){result.start=start;result.end=end;}
            continue;
        }
        if(!selected)continue;
        char key[32],unit[8];unsigned long long value;
        if(sscanf(line,"%31[^:]: %llu %7s",key,&value,unit)==3) {
            need(!strcmp(unit,"kB"),"smaps units");
            if(!strcmp(key,"Size")){result.size=value;fields|=1;}
            if(!strcmp(key,"Rss")){result.rss=value;fields|=2;}
            if(!strcmp(key,"Swap")){result.swap=value;fields|=4;}
        }
    }
    fclose(file);need(fields==7,"Size/Rss/Swap fields on selected VMA");
    need(result.size==(result.end-result.start)/1024 && result.rss+result.swap<=result.size,"real VMA sizes");
    return result;
}
static unsigned long long vm_event(const char *name) {
    FILE *file=fopen("/proc/vmstat","r");need(file!=NULL,"vmstat open");
    char key[64];unsigned long long value;
    while(fscanf(file,"%63s %llu",key,&value)==2) {
        if(!strcmp(key,name)){fclose(file);return value;}
    }
    fclose(file);need(0,"vmstat swap event");return 0;
}
static void swap_probe(volatile unsigned *mapping) {
    /* Guest-only local file fixture; never activate swap on the host/device. */
    char path[]="/tmp/thekernel-smap-swap.XXXXXX";int fd=mkstemp(path);need(fd>=0,"swap fixture file");
    need(!ftruncate(fd,2*1024*1024),"swap fixture length");
    unsigned char header[PAGE]={0};uint32_t version=1,last=511;
    memcpy(header+1024,&version,4);memcpy(header+1028,&last,4);memcpy(header+PAGE-10,"SWAPSPACE2",10);
    need(pwrite(fd,header,sizeof(header),0)==sizeof(header) && !fsync(fd),"swap header");close(fd);
    need(!swapon(path,0),"guest swap activation");
    struct observed before=observe((const void *)mapping);
    unsigned long long input=vm_event("pswpin"),output=vm_event("pswpout");
    need(!madvise((void *)mapping,PAGE*PAGES,MADV_PAGEOUT),"pageout");
    struct observed swapped=observe((const void *)mapping);
    unsigned long long output_after=vm_event("pswpout");
    need(output_after>=output+PAGES && vm_event("pswpin")==input,"submitted swap output events");
    need(swapped.swap>=before.swap+PAGE*PAGES/1024 && swapped.rss+PAGE*PAGES/1024<=before.rss,"actual software swap leaves");
    for(unsigned page=0;page<PAGES;page++)need(mapping[page*PAGE/sizeof(unsigned)]==0x12340000+page,"page-in bytes preserved");
    struct observed restored=observe((const void *)mapping);
    unsigned long long input_after=vm_event("pswpin");
    need(input_after>=input+PAGES && vm_event("pswpout")==output_after,"completed swap input events");
    printf("VMSTAT_SWAP in_delta=%llu out_delta=%llu pages\n",input_after-input,output_after-output);
    need(restored.swap+PAGE*PAGES/1024<=swapped.swap && restored.rss>=swapped.rss+PAGE*PAGES/1024,"swap leaves retire on page-in");
    need(!swapoff(path) && !unlink(path),"swap fixture cleanup");
    printf("SMAPS_SWAP before=%llu pageout=%llu restored=%llu kB\n",before.swap,swapped.swap,restored.swap);
}
static void pmap_probe(unsigned long start) {
    char pid[32],expected[32];snprintf(pid,sizeof(pid),"%ld",(long)getpid());snprintf(expected,sizeof(expected),"%016lx",start);
    int pipefd[2];need(!pipe(pipefd),"pmap pipe");pid_t child=fork();need(child>=0,"pmap fork");
    if(!child){close(pipefd[0]);dup2(pipefd[1],STDOUT_FILENO);close(pipefd[1]);execl("/opt/thekernel-tools/bin/pmap","pmap","-x",pid,(char *)NULL);_exit(127);}
    close(pipefd[1]);char output[32768];size_t length=0;ssize_t got;
    while(length<sizeof(output)-1 && (got=read(pipefd[0],output+length,sizeof(output)-1-length))>0)length+=got;
    output[length]=0;close(pipefd[0]);int status;
    need(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"real pmap exit");
    printf("%s",output);need(strstr(output,expected)!=NULL,"real pmap selected mapping row, not only totals");
}
int main(int argc,char **argv) {
    alarm(25);volatile unsigned *mapping=mmap(NULL,PAGE*PAGES,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0);
    need(mapping!=MAP_FAILED,"anonymous mapping");
    for(unsigned page=0;page<PAGES;page++)mapping[page*PAGE/sizeof(unsigned)]=0x12340000+page;
    struct observed before=observe((const void *)mapping);need(before.rss>=PAGE*PAGES/1024,"touched pages resident");
    if(argc==2 && !strcmp(argv[1],"--swap"))swap_probe(mapping);
    if(argc==2 && !strcmp(argv[1],"--tools"))pmap_probe(before.start);
    need(!munmap((void *)mapping,PAGE*PAGES),"mapping cleanup");
    puts("THEKERNEL_PROC_SMAPS_SWAP_OK");return 0;
}
