#define _GNU_SOURCE
#include <sched.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

static int fail(const char *label) {fprintf(stderr,"proc net route: %s errno=%d\n",label,errno);return 1;}
static int snapshot(int fd,char *out,size_t size) {
    if (lseek(fd,0,SEEK_SET)<0) return -1;
    ssize_t length=read(fd,out,size-1);
    if (length<0 || length==(ssize_t)size-1) return -1;
    out[length]=0;return 0;
}
static int validate_routes(char *data) {
    char *line=strtok(data,"\n");
    if (!line || !strstr(line,"Iface") || !strstr(line,"Destination") || !strstr(line,"IRTT")) return -1;
    while ((line=strtok(NULL,"\n"))) {
        char iface[32],extra;
        unsigned dst,gw,flags,mask,ref,use,metric,mtu,window,irtt;
        if (sscanf(line,"%31s %x %x %x %u %u %u %x %u %u %u %c",iface,&dst,&gw,&flags,&ref,&use,&metric,&mask,&mtu,&window,&irtt,&extra)!=11 ||
            !(flags&1) || (dst&~mask)) return -1;
    }
    return 0;
}
int main(void) {
    char link[64]={0},before[8192],after[8192],fresh[8192],parent[8192];
    ssize_t length=readlink("/proc/net",link,sizeof(link)-1);
    if (length!=8 || strcmp(link,"self/net")) return fail("net-alias");
    int ns=open("/proc/self/ns/net",O_RDONLY);
    int dir=open("/proc/self/net",O_RDONLY|O_DIRECTORY);
    int olddev=openat(dir,"dev",O_RDONLY);
    int oldroute=openat(dir,"route",O_RDONLY);
    if (ns<0 || dir<0 || olddev<0 || oldroute<0 || snapshot(olddev,before,sizeof(before)) || !strstr(before,"eth0:")) return fail("parent-network-open");
    if (snapshot(oldroute,after,sizeof(after)) || validate_routes(after)) return fail("route-format");
    if (unshare(CLONE_NEWNET)) return fail("unshare-net");
    // An already opened process-net directory follows its target task at child
    // lookup, but already opened files retain their original namespace.
    int newdev=openat(dir,"dev",O_RDONLY);
    int newroute=open("/proc/net/route",O_RDONLY);
    if (newdev<0 || newroute<0 || snapshot(newdev,fresh,sizeof(fresh)) ||
        strstr(fresh,"eth0:") || !strstr(fresh,"lo:")) return fail("new-network-view");
    if (snapshot(olddev,after,sizeof(after)) || !strstr(after,"eth0:")) return fail("opened-file-namespace-pin");
    char path[128];snprintf(path,sizeof(path),"/proc/%ld/net/dev",(long)getppid());
    int parentfd=open(path,O_RDONLY);
    if (parentfd<0 || snapshot(parentfd,parent,sizeof(parent)) || !strstr(parent,"eth0:")) return fail("target-pid-not-reader-namespace");
    if (snapshot(oldroute,after,sizeof(after)) || !strstr(after,"eth0") || validate_routes(after)) return fail("route-old-namespace-pin");
    if (snapshot(newroute,fresh,sizeof(fresh)) || strstr(fresh,"eth0") || validate_routes(fresh)) return fail("route-new-namespace");
    if (setns(ns,CLONE_NEWNET)) return fail("restore-parent-network");
    close(parentfd);close(newroute);close(newdev);close(oldroute);close(olddev);close(dir);close(ns);
    puts("THEKERNEL_PROC_NET_ROUTE_NS_OK");return 0;
}
