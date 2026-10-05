#define _GNU_SOURCE
#include <errno.h>
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
static int dump(int family, uint32_t states, ino_t inode, struct inet_diag_msg *out) {
    int fd=socket(AF_NETLINK,SOCK_RAW|SOCK_CLOEXEC,NETLINK_SOCK_DIAG);
    require(fd>=0,"diag socket");
    struct timeval timeout={3,0};
    require(!setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout)),"diag timeout");
    struct {struct nlmsghdr header; struct inet_diag_req_v2 request;} query={0};
    query.header.nlmsg_len=sizeof(query);query.header.nlmsg_type=SOCK_DIAG_BY_FAMILY;
    query.header.nlmsg_flags=NLM_F_REQUEST|NLM_F_DUMP;query.header.nlmsg_seq=57;
    query.request.sdiag_family=family;query.request.sdiag_protocol=IPPROTO_TCP;
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
    require(!listen(listener,4),"listen");
    require(dump(family,1U<<10,listener_inode,&entry)==1,"LISTEN mask and inode");
    require(entry.idiag_family==family && entry.idiag_state==10 && ntohs(entry.id.idiag_sport)==port &&
        !entry.id.idiag_dport && entry.idiag_uid==geteuid() && entry.idiag_rqueue==0 && entry.idiag_wqueue==4,"listen fields");
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
    require(dump(family,1U<<10,server_inode,&entry)==0,"established not listening");
    if(tools) real_ss(port);
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
    close(server);close(client);close(listener);
    require(dump(family,1U<<10,listener_inode,&entry)==0,"closed listener retired");
}

int main(int argc,char **argv) {
    alarm(25);int tools=argc==2 && !strcmp(argv[1],"--tools");
    check_family(AF_INET,tools);check_family(AF_INET6,tools);
    if(!geteuid()) {
        pid_t child=fork();require(child>=0,"uid child");
        if(!child) {require(!setuid(1000),"unprivileged UID");check_family(AF_INET,0);_exit(0);}
        int status;require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"nonroot socket owner");
    }
    puts("THEKERNEL_SOCKET_DIAG_OK");return 0;
}
