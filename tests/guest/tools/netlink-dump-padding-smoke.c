#define _GNU_SOURCE
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

int main(void) {
    int fd=socket(AF_NETLINK,SOCK_RAW,NETLINK_ROUTE);
    if (fd<0) {perror("netlink socket");return 1;}
    struct timeval timeout={2,0};
    if (setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout))) return 2;
    struct sockaddr_nl peer={.nl_family=AF_NETLINK};
    _Alignas(struct nlmsghdr) unsigned char request[156]={0};
    struct nlmsghdr *header=(struct nlmsghdr *)request;
    header->nlmsg_len=NLMSG_LENGTH(sizeof(struct rtmsg))+RTA_LENGTH(sizeof(unsigned));
    header->nlmsg_type=RTM_GETROUTE;header->nlmsg_flags=NLM_F_REQUEST|NLM_F_DUMP;header->nlmsg_seq=31;
    struct rtmsg *route=NLMSG_DATA(header);route->rtm_family=AF_INET;
    struct rtattr *attribute=(struct rtattr *)(request+NLMSG_LENGTH(sizeof(*route)));
    attribute->rta_type=RTA_TABLE;attribute->rta_len=RTA_LENGTH(sizeof(unsigned));
    unsigned table=RT_TABLE_MAIN;memcpy(RTA_DATA(attribute),&table,sizeof(table));
    // Match the real Alpine iproute2 allocation: only the first 36 bytes are
    // a message, while sendto submits the entire zero-initialized 156 bytes.
    if (sendto(fd,request,sizeof(request),0,(struct sockaddr *)&peer,sizeof(peer))!=(ssize_t)sizeof(request)) {perror("padded dump send");return 3;}
    _Alignas(struct nlmsghdr) unsigned char reply[8192];unsigned rows=0;int done=0;
    for (unsigned tries=0;tries<32 && !done;tries++) {
        ssize_t received=recv(fd,reply,sizeof(reply),0);
        if (received<0) {perror("dump recv");return 4;}
        int remaining=(int)received;
        for (struct nlmsghdr *msg=(struct nlmsghdr *)reply;NLMSG_OK(msg,remaining);msg=NLMSG_NEXT(msg,remaining)) {
            if (msg->nlmsg_seq!=31) continue;
            if (msg->nlmsg_type==NLMSG_ERROR) return 5;
            if (msg->nlmsg_type==NLMSG_DONE) done=1;
            if (msg->nlmsg_type==RTM_NEWROUTE) rows++;
        }
    }
    close(fd);
    if (!done) return 6;
    printf("THEKERNEL_NETLINK_DUMP_PADDING_OK rows=%u\n",rows);return 0;
}
