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
            if (msg->nlmsg_type==NLMSG_DONE) {
                int status;
                if (msg->nlmsg_len<NLMSG_LENGTH(sizeof(status)) || !(msg->nlmsg_flags&NLM_F_MULTI)) return 7;
                memcpy(&status,NLMSG_DATA(msg),sizeof(status));
                if (status) return 8;
                done=1;
            }
            if (msg->nlmsg_type==RTM_NEWROUTE) {
                if (msg->nlmsg_len<NLMSG_LENGTH(sizeof(struct rtmsg))) return 9;
                struct rtmsg *entry=NLMSG_DATA(msg);
                int attrlen=RTM_PAYLOAD(msg);
                for (struct rtattr *attr=RTM_RTA(entry);RTA_OK(attr,attrlen);attr=RTA_NEXT(attr,attrlen)) {
                    if (attr->rta_type==RTA_DST && entry->rtm_family==AF_INET && RTA_PAYLOAD(attr)==4) {
                        const unsigned char *address=RTA_DATA(attr);
                        unsigned prefix=entry->rtm_dst_len;
                        for (unsigned byte=0;byte<4;byte++) {
                            unsigned used=prefix>byte*8 ? prefix-byte*8 : 0;
                            if (used>8) used=8;
                            unsigned mask=used ? (0xff << (8-used))&0xff : 0;
                            if (address[byte]&~mask) return 10;
                        }
                    }
                }
                rows++;
            }
        }
    }
    close(fd);
    if (!done) return 6;
    printf("THEKERNEL_NETLINK_DUMP_PADDING_OK rows=%u\n",rows);return 0;
}
