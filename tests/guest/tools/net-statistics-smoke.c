#define _GNU_SOURCE
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

struct counts {uint64_t rx_packets,tx_packets,rx_bytes,tx_bytes,rx_errors,tx_errors,rx_dropped,tx_dropped;};
static void need(int good,const char *name) {if(!good){fprintf(stderr,"net-statistics: %s errno=%d (%s)\n",name,errno,strerror(errno));exit(1);}}
static uint64_t scalar(const char *root,const char *name) {
    char path[256];snprintf(path,sizeof(path),"%s/class/net/lo/statistics/%s",root,name);
    FILE *file=fopen(path,"r");need(file!=NULL,"sysfs counter open");
    unsigned long long value;char newline;
    need(fscanf(file,"%llu%c",&value,&newline)==2 && newline=='\n' && fgetc(file)==EOF,"sysfs decimal newline");
    fclose(file);return value;
}
static struct counts sys_counts(const char *root) {
    return (struct counts){scalar(root,"rx_packets"),scalar(root,"tx_packets"),scalar(root,"rx_bytes"),scalar(root,"tx_bytes"),
        scalar(root,"rx_errors"),scalar(root,"tx_errors"),scalar(root,"rx_dropped"),scalar(root,"tx_dropped")};
}
static struct counts proc_counts(void) {
    FILE *file=fopen("/proc/net/dev","r");need(file!=NULL,"proc counter open");char line[512];struct counts c={0};int found=0;
    while(fgets(line,sizeof(line),file)) {
        char name[32];unsigned long long v[16];
        if(sscanf(line," %31[^:]: %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu",name,
            &v[0],&v[1],&v[2],&v[3],&v[4],&v[5],&v[6],&v[7],&v[8],&v[9],&v[10],&v[11],&v[12],&v[13],&v[14],&v[15])==17 && !strcmp(name,"lo")) {
            c=(struct counts){v[1],v[9],v[0],v[8],v[2],v[10],v[3],v[11]};found=1;break;
        }
    }
    fclose(file);need(found,"proc loopback row");return c;
}
static struct counts route_counts(void) {
    int fd=socket(AF_NETLINK,SOCK_RAW|SOCK_CLOEXEC,NETLINK_ROUTE);need(fd>=0,"route socket");
    struct timeval timeout={3,0};need(!setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"route timeout");
    struct {struct nlmsghdr hdr;struct ifinfomsg info;} query={0};
    query.hdr.nlmsg_len=sizeof(query);query.hdr.nlmsg_type=RTM_GETLINK;query.hdr.nlmsg_flags=NLM_F_REQUEST|NLM_F_DUMP;query.hdr.nlmsg_seq=51;
    struct sockaddr_nl kernel={.nl_family=AF_NETLINK};need(sendto(fd,&query,sizeof(query),0,(void *)&kernel,sizeof(kernel))==sizeof(query),"route send");
    struct counts c={0};int found=0,done=0;
    for(int attempt=0;attempt<32 && !done;attempt++) {
        _Alignas(struct nlmsghdr) char reply[16384];int length=recv(fd,reply,sizeof(reply),0);need(length>0,"route recv");
        for(struct nlmsghdr *hdr=(void *)reply;NLMSG_OK(hdr,length);hdr=NLMSG_NEXT(hdr,length)) {
            need(hdr->nlmsg_type!=NLMSG_ERROR,"route error");if(hdr->nlmsg_type==NLMSG_DONE){done=1;continue;}
            if(hdr->nlmsg_type!=RTM_NEWLINK)continue;
            struct ifinfomsg *info=NLMSG_DATA(hdr);int alen=IFLA_PAYLOAD(hdr);int lo=0;const void *wide=NULL,*narrow=NULL;
            for(struct rtattr *attr=IFLA_RTA(info);RTA_OK(attr,alen);attr=RTA_NEXT(attr,alen)) {
                if(attr->rta_type==IFLA_IFNAME && RTA_PAYLOAD(attr)>=3 && !strcmp(RTA_DATA(attr),"lo"))lo=1;
                if(attr->rta_type==IFLA_STATS64 && RTA_PAYLOAD(attr)>=sizeof(c))wide=RTA_DATA(attr);
                if(attr->rta_type==IFLA_STATS && RTA_PAYLOAD(attr)>=8*sizeof(uint32_t))narrow=RTA_DATA(attr);
            }
            if(lo){need(wide && narrow,"both link stat attributes");memcpy(&c,wide,sizeof(c));uint32_t small[8];memcpy(small,narrow,sizeof(small));uint64_t big[8];memcpy(big,&c,sizeof(big));
                for(int i=0;i<8;i++){need(small[i]==(uint32_t)big[i],"32/64 counters agree");}
                found=1;}
        }
    }
    close(fd);need(done && found,"link dump completion and lo");return c;
}
static struct counts synchronized_counts(const char *root) {
    for(int retry=0;retry<100;retry++) {
        struct counts a=sys_counts(root),b=proc_counts(),c=route_counts();
        if(!memcmp(&a,&b,sizeof(a)) && !memcmp(&a,&c,sizeof(a)))return a;
        usleep(10000);
    }
    need(0,"quiescent sysfs/proc/netlink agreement");return (struct counts){0};
}
static void traffic(void) {
    int receiver=socket(AF_INET,SOCK_DGRAM|SOCK_CLOEXEC,0),sender=socket(AF_INET,SOCK_DGRAM|SOCK_CLOEXEC,0);need(receiver>=0 && sender>=0,"UDP sockets");
    struct timeval timeout={3,0};need(!setsockopt(receiver,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"UDP timeout");
    struct sockaddr_in address={.sin_family=AF_INET,.sin_addr.s_addr=htonl(INADDR_LOOPBACK)};socklen_t length=sizeof(address);
    need(!bind(receiver,(void *)&address,length) && !getsockname(receiver,(void *)&address,&length),"UDP bind");
    char data[512]={0},received[512];for(int packet=0;packet<5;packet++) {
        need(sendto(sender,data,sizeof(data),0,(void *)&address,length)==sizeof(data),"UDP send");
        need(recv(receiver,received,sizeof(received),0)==sizeof(received) && !memcmp(data,received,sizeof(data)),"UDP received");
    }
    close(sender);close(receiver);
}
static void advance(const char *root) {
    struct counts before=synchronized_counts(root);traffic();struct counts after=synchronized_counts(root);
    need(after.rx_packets>=before.rx_packets+5 && after.tx_packets>=before.tx_packets+5 &&
        after.rx_bytes>=before.rx_bytes+2560 && after.tx_bytes>=before.tx_bytes+2560,"actual traffic advances counters");
    printf("NET_STATS lo rx=%llu/%llu tx=%llu/%llu\n",(unsigned long long)after.rx_bytes,(unsigned long long)after.rx_packets,
        (unsigned long long)after.tx_bytes,(unsigned long long)after.tx_packets);
}
/* Test setup only: Linux creates lo down and without addresses in NEWNET. */
static void configure_new_loopback(void) {
    int fd=socket(AF_NETLINK,SOCK_RAW|SOCK_CLOEXEC,NETLINK_ROUTE);need(fd>=0,"configure route socket");
    struct timeval timeout={3,0};need(!setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"configure timeout");
    struct sockaddr_nl kernel={.nl_family=AF_NETLINK};unsigned index=if_nametoindex("lo");need(index!=0,"new lo ifindex");
    for(int pass=0;pass<2;pass++) {
        _Alignas(struct nlmsghdr) char request[128]={0};struct nlmsghdr *hdr=(void *)request;
        hdr->nlmsg_flags=NLM_F_REQUEST|NLM_F_ACK;hdr->nlmsg_seq=60+pass;
        if(!pass) {
            hdr->nlmsg_type=RTM_NEWADDR;hdr->nlmsg_flags|=NLM_F_CREATE|NLM_F_EXCL;
            struct ifaddrmsg *address=NLMSG_DATA(hdr);address->ifa_family=AF_INET;address->ifa_prefixlen=8;
            address->ifa_scope=RT_SCOPE_HOST;address->ifa_index=index;
            hdr->nlmsg_len=NLMSG_LENGTH(sizeof(*address))+RTA_LENGTH(sizeof(uint32_t));
            struct rtattr *attr=(void *)(request+NLMSG_LENGTH(sizeof(*address)));attr->rta_type=IFA_LOCAL;attr->rta_len=RTA_LENGTH(sizeof(uint32_t));
            uint32_t local=htonl(INADDR_LOOPBACK);memcpy(RTA_DATA(attr),&local,sizeof(local));
        } else {
            hdr->nlmsg_type=RTM_SETLINK;hdr->nlmsg_len=NLMSG_LENGTH(sizeof(struct ifinfomsg));
            struct ifinfomsg *info=NLMSG_DATA(hdr);info->ifi_index=index;info->ifi_flags=IFF_UP;info->ifi_change=IFF_UP;
        }
        need(sendto(fd,request,hdr->nlmsg_len,0,(void *)&kernel,sizeof(kernel))==hdr->nlmsg_len,"configure lo send");
        _Alignas(struct nlmsghdr) char reply[512];int length=recv(fd,reply,sizeof(reply),0);need(length>0,"configure lo ack");int ack=0;
        for(struct nlmsghdr *msg=(void *)reply;NLMSG_OK(msg,length);msg=NLMSG_NEXT(msg,length)) {
            if(msg->nlmsg_type==NLMSG_ERROR && msg->nlmsg_seq==hdr->nlmsg_seq) {
                need(msg->nlmsg_len>=NLMSG_LENGTH(sizeof(struct nlmsgerr)),"configure ack shape");
                struct nlmsgerr *error=NLMSG_DATA(msg);need(!error->error,"configure lo accepted");ack=1;
            }
        }
        need(ack,"configure lo matching ack");
    }
    close(fd);
}
static void namespaces(void) {
    uint64_t old=scalar("/sys","rx_bytes");need(old>0,"initial traffic");
    need(!unshare(CLONE_NEWNET|CLONE_NEWNS),"new net/mount namespace");
    need(scalar("/sys","rx_bytes")==old,"old sysfs pins original namespace");
    need(proc_counts().rx_bytes==0,"new proc namespace starts empty");
    char path[]="/tmp/thekernel-net-stat-sys.XXXXXX";need(mkdtemp(path)!=NULL,"sysfs directory");
    need(!mount("sysfs",path,"sysfs",0,NULL),"new sysfs mount");need(scalar(path,"rx_bytes")==0,"new sysfs captures new namespace");
    configure_new_loopback();advance(path);need(scalar("/sys","rx_bytes")==old,"new traffic does not alter old sysfs");
    need(!umount(path) && !rmdir(path),"new sysfs cleanup");
}
static void tools(void) {
    for(int busy=0;busy<2;busy++) {
        pid_t pid=fork();need(pid>=0,"ip fork");if(!pid) {
            if(busy)execl("/opt/thekernel-tools/bin/busybox","busybox","ifconfig","lo",(char *)NULL);
            else execl("/opt/thekernel-tools/bin/ip","ip","-s","link","show","lo",(char *)NULL);
            _exit(127);
        }
        int status;need(waitpid(pid,&status,0)==pid && WIFEXITED(status) && !WEXITSTATUS(status),"actual ip statistics exit");
    }
}
int main(int argc,char **argv) {
    alarm(25);advance("/sys");
    if(argc==2 && !strcmp(argv[1],"--namespace"))namespaces();
    if(argc==2 && !strcmp(argv[1],"--tools"))tools();
    puts("THEKERNEL_NET_STATISTICS_OK");return 0;
}
