#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <linux/inet_diag.h>
#include <linux/netlink.h>
#include <linux/sock_diag.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int good, const char *name) {
    if (!good) { fprintf(stderr,"socket-diag: %s errno=%d (%s)\n",name,errno,strerror(errno)); exit(1); }
}

static uint32_t request_cookie;
static int dump_protocol(int family, unsigned protocol, uint32_t states, ino_t inode, struct inet_diag_msg *out) {
    int fd=socket(AF_NETLINK,SOCK_RAW|SOCK_CLOEXEC,NETLINK_SOCK_DIAG);
    require(fd>=0,"diag socket");
    struct timeval timeout={3,0};
    require(!setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"diag timeout");
    struct {struct nlmsghdr header; struct inet_diag_req_v2 request;} query={0};
    query.header.nlmsg_len=sizeof(query);query.header.nlmsg_type=SOCK_DIAG_BY_FAMILY;
    query.header.nlmsg_flags=NLM_F_REQUEST|NLM_F_DUMP;query.header.nlmsg_seq=57;
    query.request.sdiag_family=family;query.request.sdiag_protocol=protocol;
    query.request.idiag_states=states;
    query.request.id.idiag_cookie[0]=query.request.id.idiag_cookie[1]=request_cookie;
    struct sockaddr_nl kernel={.nl_family=AF_NETLINK};
    require(sendto(fd,&query,sizeof(query),0,(void *)&kernel,sizeof(kernel))==(ssize_t)sizeof(query),"diag send");
    int found=0,done=0;
    for (int batch=0;batch<64 && !done;batch++) {
        _Alignas(struct nlmsghdr) char reply[16384];
        ssize_t size=recv(fd,reply,sizeof(reply),0);require(size>0,"diag recv");
        int remaining=size;
        for (struct nlmsghdr *hdr=(void *)reply;NLMSG_OK(hdr,remaining);hdr=NLMSG_NEXT(hdr,remaining)) {
            require(hdr->nlmsg_seq==57,"diag sequence");
            require(hdr->nlmsg_type!=NLMSG_ERROR,"diag error");
            if(hdr->nlmsg_type==NLMSG_DONE) {done=1;continue;}
            require(hdr->nlmsg_type==SOCK_DIAG_BY_FAMILY && hdr->nlmsg_len>=NLMSG_LENGTH(sizeof(*out)),"diag shape");
            struct inet_diag_msg *entry=NLMSG_DATA(hdr);
            if (entry->idiag_inode==(uint32_t)inode) {*out=*entry;found++;}
        }
    }
    close(fd);require(done,"diag completion");return found;
}

static int dump(int family, uint32_t states, ino_t inode, struct inet_diag_msg *out) {
    return dump_protocol(family,IPPROTO_TCP,states,inode,out);
}

static int proc_tcp_rows(FILE *file,int family,ino_t inode,struct inet_diag_msg *out) {
    char line[512];require(fgets(line,sizeof(line),file)!=NULL && strstr(line,"local_address") && strstr(line,"inode"),"proc TCP header");
    int found=0;
    while(fgets(line,sizeof(line),file)) {
        unsigned slot,sport,dport,state,sendq,recvq,timer,remaining,retries,uid,probes;
        unsigned long long number;char local[65],peer[65];
        require(sscanf(line,"%u: %64[0-9A-F]:%x %64[0-9A-F]:%x %x %x:%x %x:%x %x %u %u %llu",&slot,local,&sport,peer,&dport,
            &state,&sendq,&recvq,&timer,&remaining,&retries,&uid,&probes,&number)==14,"proc TCP mandatory grammar");
        if(number!=(unsigned long long)inode)continue;
        memset(out,0,sizeof(*out));out->idiag_family=family;out->idiag_state=state;out->idiag_rqueue=recvq;out->idiag_wqueue=sendq;
        out->idiag_uid=uid;out->idiag_inode=number;out->id.idiag_sport=htons(sport);out->id.idiag_dport=htons(dport);
        require(strlen(local)==(family==AF_INET?8:32) && strlen(peer)==strlen(local),"proc address word width");
        for(unsigned word=0;word<(family==AF_INET?1U:4U);word++) {
            char hex[9]={0};memcpy(hex,local+word*8,8);out->id.idiag_src[word]=strtoul(hex,NULL,16);
            memcpy(hex,peer+word*8,8);out->id.idiag_dst[word]=strtoul(hex,NULL,16);
        }
        found++;
    }
    return found;
}
static int proc_tcp(int family,ino_t inode,struct inet_diag_msg *out) {
    FILE *file=fopen(family==AF_INET?"/proc/net/tcp":"/proc/net/tcp6","r");require(file!=NULL,"proc TCP open");
    int found=proc_tcp_rows(file,family,inode,out);fclose(file);return found;
}
static void namespace_views(ino_t inode) {
    int saved=open("/proc/self/ns/net",O_RDONLY|O_CLOEXEC);require(saved>=0,"saved net namespace");
    FILE *old=fopen("/proc/net/tcp","r");require(old!=NULL,"old TCP file");
    require(!unshare(CLONE_NEWNET),"new network namespace");
    struct inet_diag_msg entry={0};
    require(proc_tcp_rows(old,AF_INET,inode,&entry)==1,"opened TCP file pins old namespace");
    require(proc_tcp(AF_INET,inode,&entry)==0,"fresh TCP file selects new namespace");
    require(!setns(saved,CLONE_NEWNET),"restore network namespace");
    require(proc_tcp(AF_INET,inode,&entry)==1,"restored namespace exposes original endpoint");
    fclose(old);close(saved);
}
static void compare_proc(int family,ino_t inode,const struct inet_diag_msg *diag) {
    struct inet_diag_msg entry={0};require(proc_tcp(family,inode,&entry)==1,"proc TCP inode discovery");
    require(entry.idiag_state==diag->idiag_state && entry.idiag_uid==diag->idiag_uid && entry.idiag_rqueue==diag->idiag_rqueue &&
        entry.idiag_wqueue==(diag->idiag_state==10?0:diag->idiag_wqueue) &&
        !memcmp(&entry.id.idiag_src,&diag->id.idiag_src,16) && !memcmp(&entry.id.idiag_dst,&diag->id.idiag_dst,16) &&
        entry.id.idiag_sport==diag->id.idiag_sport && entry.id.idiag_dport==diag->id.idiag_dport,"proc TCP real endpoint/state/owner/queue agreement");
}
static void real_netstat(unsigned port, int busybox) {
    int pipefd[2];require(!pipe(pipefd),"netstat output pipe");pid_t child=fork();require(child>=0,"netstat fork");
    if(!child){close(pipefd[0]);dup2(pipefd[1],STDOUT_FILENO);close(pipefd[1]);if(busybox)execl("/opt/thekernel-tools/bin/busybox","busybox","netstat","-tanp",(char *)NULL);
        else execl("/opt/thekernel-tools/bin/netstat","netstat","-tanp",(char *)NULL);
        _exit(127);
    }
    close(pipefd[1]);char text[16384];size_t length=0;ssize_t size;
    while(length<sizeof(text)-1 && (size=read(pipefd[0],text+length,sizeof(text)-1-length))>0)length+=size;
    text[length]=0;close(pipefd[0]);int status;require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"real TCP netstat exit");
    printf("NETSTAT_PROVIDER=%s\n%s",busybox?"busybox":"net-tools",text);char endpoint[32];snprintf(endpoint,sizeof(endpoint),":%u",port);
    require(strstr(text,"LISTEN") && strstr(text,"ESTABLISHED") && strstr(text,endpoint),"real netstat live TCP rows");
    if(busybox) {
        char owner[32];snprintf(owner,sizeof(owner),"%ld/",(long)getpid());
        char *line=strstr(text,"LISTEN"),*end=strchr(line,'\n'),*pid=strstr(line,owner);
        require(pid && (!end || pid<end),"BusyBox netstat listener PID mapping including single-digit inode");
    }
}
static ino_t inode_of(int fd) {struct stat st;require(!fstat(fd,&st),"socket inode");return st.st_ino;}
static void ready(int fd) {struct pollfd p={.fd=fd,.events=POLLIN};require(poll(&p,1,3000)==1 && (p.revents&POLLIN),"receive ready");}

static void real_ss(unsigned port) {
    int pipefd[2];require(!pipe(pipefd),"ss output pipe");
    pid_t pid=fork();require(pid>=0,"ss fork");
    if(!pid) {
        close(pipefd[0]);dup2(pipefd[1],STDOUT_FILENO);close(pipefd[1]);
        execl("/opt/thekernel-tools/bin/ss","ss","-tanpeo",(char *)NULL);_exit(127);
    }
    close(pipefd[1]);char output[16384];size_t length=0;ssize_t size;
    while(length<sizeof(output)-1 && (size=read(pipefd[0],output+length,sizeof(output)-1-length))>0) length+=size;
    output[length]=0;close(pipefd[0]);int status;
    require(waitpid(pid,&status,0)==pid && WIFEXITED(status) && WEXITSTATUS(status)==0,"real ss exit");
    printf("%s",output);char endpoint[32];snprintf(endpoint,sizeof(endpoint),":%u",port);
    require(strstr(output,"LISTEN") && strstr(output,"ESTAB") && strstr(output,endpoint),"real ss active endpoints");
}

static int namespace_mode;
static void check_family(int family, int tools) {
    int listener=socket(family,SOCK_STREAM|SOCK_CLOEXEC,IPPROTO_TCP);
    require(listener>=0,"listener socket");
    struct sockaddr_storage address={0};socklen_t length;
    if(family==AF_INET) {
        struct sockaddr_in *a=(void *)&address;a->sin_family=family;a->sin_addr.s_addr=htonl(INADDR_LOOPBACK);length=sizeof(*a);
    } else {
        struct sockaddr_in6 *a=(void *)&address;a->sin6_family=family;a->sin6_addr=in6addr_loopback;length=sizeof(*a);
    }
    require(!bind(listener,(void *)&address,length),"listen bind");
    require(!getsockname(listener,(void *)&address,&length),"listen address");
    unsigned port=family==AF_INET?ntohs(((struct sockaddr_in *)&address)->sin_port):ntohs(((struct sockaddr_in6 *)&address)->sin6_port);
    ino_t listener_inode=inode_of(listener);struct inet_diag_msg entry={0};
    require(dump(family,1U<<13,listener_inode,&entry)==1 && entry.idiag_state==7,"bound inactive pseudo-state mask");
    require(dump(family,1U<<7,listener_inode,&entry)==0,"bound inactive not CLOSE mask");
    require(proc_tcp(family,listener_inode,&entry)==0,"proc TCP omits bound inactive OFD");
    require(!listen(listener,4),"listen");
    require(dump(family,1U<<10,listener_inode,&entry)==1,"LISTEN mask and inode");
    require(entry.idiag_family==family && entry.idiag_state==10 && ntohs(entry.id.idiag_sport)==port &&
        !entry.id.idiag_dport && entry.idiag_uid==geteuid() && entry.idiag_rqueue==0 && entry.idiag_wqueue==4,"listen fields");
    compare_proc(family,listener_inode,&entry);
    if(family==AF_INET) require(entry.id.idiag_src[0]==htonl(INADDR_LOOPBACK),"IPv4 address bytes");
    else require(!memcmp(entry.id.idiag_src,&in6addr_loopback,16),"IPv6 address bytes");
    request_cookie=0x5a5a5a5a;
    require(dump(family,1U<<10,listener_inode,&entry)==1,"dump cookie is not exact lookup filter");
    request_cookie=0;
    require(dump(family,1U<<9,listener_inode,&entry)==0,"adjacent state bit excluded");
    require(dump(family,0,listener_inode,&entry)==0,"zero state mask excluded");
    int client=socket(family,SOCK_STREAM|SOCK_CLOEXEC,IPPROTO_TCP);require(client>=0,"client socket");
    require(!connect(client,(void *)&address,length),"connect");ready(listener);
    require(dump(family,1U<<10,listener_inode,&entry)==1 && entry.idiag_rqueue==1,"accept backlog observed");
    int server=accept4(listener,NULL,NULL,SOCK_CLOEXEC);require(server>=0,"accept");
    const char data[]="queued-for-diag";
    require(send(client,data,sizeof(data),0)==sizeof(data),"send queued data");ready(server);
    ino_t server_inode=inode_of(server);
    require(dump(family,1U<<1,server_inode,&entry)==1,"ESTABLISHED mask and inode");
    require(entry.idiag_state==1 && ntohs(entry.id.idiag_sport)==port && entry.id.idiag_dport &&
        entry.idiag_uid==geteuid() && entry.idiag_rqueue==sizeof(data),"connected fields and real queue");
    compare_proc(family,server_inode,&entry);
    require(dump(family,1U<<10,server_inode,&entry)==0,"established not listening");
    if(tools) {
        real_ss(port);
        require(inode_of(listener)==listener_inode && inode_of(server)==server_inode,"tools preserve parent descriptors");
        real_netstat(port,0);real_netstat(port,1);
    }
    char received[sizeof(data)];require(recv(server,received,sizeof(received),MSG_WAITALL)==sizeof(received) &&
        !memcmp(received,data,sizeof(data)),"diagnostics did not consume data");
    require(dump(family,1U<<1,server_inode,&entry)==1 && entry.idiag_rqueue==0,"queue drained");
    int keepalive=1;
    require(!setsockopt(server,SOL_SOCKET,SO_KEEPALIVE,&keepalive,sizeof(keepalive)),"keepalive activation");
    require(send(client,"k",1,0)==1,"keepalive observation traffic");ready(server);
    char keepalive_byte;require(recv(server,&keepalive_byte,1,0)==1 && keepalive_byte=='k',"keepalive observation received");
    require(dump(family,1U<<1,server_inode,&entry)==1 && entry.idiag_timer==2 && entry.idiag_expires>0 &&
        !entry.idiag_retrans,"actual keepalive timer and expiry");
    if(tools)real_ss(port);
    if(namespace_mode && family==AF_INET && !geteuid())namespace_views(server_inode);
    close(server);close(client);close(listener);
    require(dump(family,1U<<10,listener_inode,&entry)==0,"closed listener retired");
    require(proc_tcp(family,listener_inode,&entry)==0,"proc closed listener retired");
}

static int proc_udp(int family, ino_t inode, struct inet_diag_msg *out) {
    FILE *file=fopen(family==AF_INET?"/proc/net/udp":"/proc/net/udp6","r");require(file!=NULL,"proc UDP open");
    int found=proc_tcp_rows(file,family,inode,out);fclose(file);return found;
}
static void udp_fields(int family,int fd,unsigned state,struct inet_diag_msg *diag) {
    ino_t inode=inode_of(fd);require(dump_protocol(family,IPPROTO_UDP,1U<<state,inode,diag)==1,"UDP diag inode/state discovery");
    struct inet_diag_msg proc={0};require(proc_udp(family,inode,&proc)==1,"proc UDP inode discovery");
    require(proc.idiag_state==state && proc.idiag_uid==geteuid() && proc.idiag_inode==diag->idiag_inode &&
        proc.idiag_rqueue==diag->idiag_rqueue && proc.idiag_wqueue==diag->idiag_wqueue &&
        !memcmp(&proc.id,&diag->id,36),"proc UDP endpoint state queues owner agreement");
    require(!diag->idiag_timer && !diag->idiag_expires && !diag->idiag_retrans,"UDP has no TCP retransmission timer");
}
static void real_udp_tools(unsigned port) {
    for(int provider=0;provider<3;provider++) {
        int pipefd[2];require(!pipe(pipefd),"UDP tool pipe");pid_t child=fork();require(child>=0,"UDP tool fork");
        if(!child) {
            close(pipefd[0]);dup2(pipefd[1],STDOUT_FILENO);close(pipefd[1]);
            if(provider==0)execl("/opt/thekernel-tools/bin/ss","ss","-uanpe",(char *)NULL);
            else if(provider==1)execl("/opt/thekernel-tools/bin/netstat","netstat","-uanp",(char *)NULL);
            else execl("/opt/thekernel-tools/bin/busybox","busybox","netstat","-uanp",(char *)NULL);
            _exit(127);
        }
        close(pipefd[1]);char text[16384];size_t length=0;ssize_t n;
        while(length<sizeof(text)-1 && (n=read(pipefd[0],text+length,sizeof(text)-1-length))>0)length+=n;
        text[length]=0;close(pipefd[0]);int status;
        require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"real UDP tool exit");
        printf("UDP_PROVIDER=%d\n%s",provider,text);char endpoint[32];snprintf(endpoint,sizeof(endpoint),":%u",port);
        require(strstr(text,endpoint) && strstr(text,"ESTAB"),"real UDP tools active endpoints");
    }
}
static void check_udp(int family,int tools) {
    int server=socket(family,SOCK_DGRAM|SOCK_CLOEXEC,IPPROTO_UDP),client=socket(family,SOCK_DGRAM|SOCK_CLOEXEC,IPPROTO_UDP);
    require(server>=0 && client>=0,"UDP sockets");struct inet_diag_msg entry={0};
    require(dump_protocol(family,IPPROTO_UDP,~0U,inode_of(server),&entry)==0,"unbound UDP omitted");
    struct sockaddr_storage address={0};socklen_t size;
    if(family==AF_INET){struct sockaddr_in *a=(void *)&address;a->sin_family=family;a->sin_addr.s_addr=htonl(INADDR_LOOPBACK);size=sizeof(*a);}
    else {struct sockaddr_in6 *a=(void *)&address;a->sin6_family=family;a->sin6_addr=in6addr_loopback;size=sizeof(*a);}
    require(!bind(server,(void *)&address,size) && !getsockname(server,(void *)&address,&size),"UDP bound address");
    unsigned port=family==AF_INET?ntohs(((struct sockaddr_in *)&address)->sin_port):ntohs(((struct sockaddr_in6 *)&address)->sin6_port);
    udp_fields(family,server,7,&entry);require(!entry.idiag_rqueue && !entry.idiag_wqueue,"empty UDP queue");
    require(dump_protocol(family,IPPROTO_UDP,1U<<13,inode_of(server),&entry)==0,"UDP not TCP bound-inactive pseudo-state");
    require(!connect(client,(void *)&address,size),"UDP connect");udp_fields(family,client,1,&entry);
    require(ntohs(entry.id.idiag_dport)==port,"UDP connected peer port");
    const char data[]="udp-first",second[]="udp-next";
    require(send(client,data,sizeof(data),0)==sizeof(data) && send(client,second,sizeof(second),0)==sizeof(second),"UDP queued datagrams");ready(server);
    udp_fields(family,server,7,&entry);require(entry.idiag_rqueue>=sizeof(data)+sizeof(second),"UDP queue includes all unread datagrams");
    if(tools)real_udp_tools(port);
    char bytes[32];require(recv(server,bytes,sizeof(bytes),0)==sizeof(data) && !memcmp(bytes,data,sizeof(data)),"UDP diagnostics preserve first datagram");
    require(recv(server,bytes,sizeof(bytes),0)==sizeof(second) && !memcmp(bytes,second,sizeof(second)),"UDP diagnostics preserve second datagram");
    udp_fields(family,server,7,&entry);require(!entry.idiag_rqueue,"UDP queue drained");
    require(send(client,"corked",6,MSG_MORE)==6,"UDP cork");udp_fields(family,client,1,&entry);require(entry.idiag_wqueue>=6,"UDP cork queue observed");
    require(send(client,"!",1,0)==1,"UDP cork flush");ready(server);
    require(recv(server,bytes,sizeof(bytes),0)==7 && !memcmp(bytes,"corked!",7),"UDP cork unchanged by diagnostics");
    if(namespace_mode && family==AF_INET && !geteuid()) {
        int ns=open("/proc/self/ns/net",O_RDONLY|O_CLOEXEC);require(ns>=0,"saved UDP namespace");
        FILE *old=fopen("/proc/net/udp","r");require(old!=NULL,"old UDP file");
        require(!unshare(CLONE_NEWNET),"new UDP namespace");
        require(proc_tcp_rows(old,family,inode_of(server),&entry)==1 && proc_udp(family,inode_of(server),&entry)==0,"UDP old file pins namespace fresh file selects new");
        require(!setns(ns,CLONE_NEWNET),"restore UDP namespace");fclose(old);close(ns);
    }
    ino_t inode=inode_of(server);close(server);close(client);
    require(dump_protocol(family,IPPROTO_UDP,~0U,inode,&entry)==0 && proc_udp(family,inode,&entry)==0,"UDP final close retires endpoint");
}

int main(int argc,char **argv) {
    alarm(25);int tools=argc==2 && !strcmp(argv[1],"--tools");
    namespace_mode=argc==2 && !strcmp(argv[1],"--namespace");
    check_family(AF_INET,tools);check_family(AF_INET6,tools);
    check_udp(AF_INET,tools);check_udp(AF_INET6,tools);
    if(!geteuid()) {
        pid_t child=fork();require(child>=0,"uid child");
        if(!child) {require(!setuid(1000),"unprivileged UID");check_family(AF_INET,0);check_udp(AF_INET,0);_exit(0);}
        int status;require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"nonroot socket owner");
    }
    puts("THEKERNEL_SOCKET_DIAG_OK");return 0;
}
