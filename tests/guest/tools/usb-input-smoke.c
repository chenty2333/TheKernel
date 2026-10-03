#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>
static int bit(unsigned char *bits, int code) { return (bits[code/8] >> (code%8)) & 1; }
int main(void) {
    struct pollfd files[32]; int count=0, keyboard=0, relative=0, absolute=0;
    for (int i=0;i<32;i++) {
        char path[64]; snprintf(path,sizeof path,"/dev/input/event%d",i);
        int fd=open(path,O_RDONLY|O_NONBLOCK); if(fd<0) continue;
        struct input_id id; unsigned char bits[128]={0};
        if(ioctl(fd,EVIOCGID,&id)<0 || id.bustype!=BUS_USB) { close(fd); continue; }
        if(ioctl(fd,EVIOCGBIT(EV_KEY,sizeof bits),bits)>=0 && bit(bits,KEY_A)) keyboard++;
        memset(bits,0,sizeof bits);
        if(ioctl(fd,EVIOCGBIT(EV_REL,sizeof bits),bits)>=0 && bit(bits,REL_X)) relative++;
        memset(bits,0,sizeof bits);
        if(ioctl(fd,EVIOCGBIT(EV_ABS,sizeof bits),bits)>=0 && bit(bits,ABS_X)) absolute++;
        files[count++]=(struct pollfd){fd,POLLIN,0};
    }
    if(!keyboard||!relative||!absolute) { fprintf(stderr,"USB capabilities missing: kbd=%d rel=%d abs=%d\n",keyboard,relative,absolute); return 1; }
    puts("\nN305_USB_INPUT_WAIT"); fflush(stdout);
    int key=0,rel=0,abs=0; time_t end=time(NULL)+15;
    while(time(NULL)<end && !(key&&rel&&abs)) {
        if(poll(files,(nfds_t)count,100)<0 && errno!=EINTR) return 1;
        for(int i=0;i<count;i++) {
            struct input_event events[16]; ssize_t bytes=read(files[i].fd,events,sizeof events);
            if(bytes<0) continue;
            for(size_t j=0;j<(size_t)bytes/sizeof events[0];j++) {
                if(events[j].type==EV_KEY && events[j].code==KEY_A && events[j].value==1) key=1;
                if(events[j].type==EV_REL && events[j].code==REL_X && events[j].value) rel=1;
                if(events[j].type==EV_ABS && events[j].code==ABS_X && events[j].value) abs=1;
            }
        }
    }
    for(int i=0;i<count;i++) close(files[i].fd);
    if(!(key&&rel&&abs)) { fprintf(stderr,"USB events missing: key=%d rel=%d abs=%d\n",key,rel,abs); return 1; }
    puts("\nN305_USB_INPUT_PASS keyboard relative absolute"); fflush(stdout); return 0;
}
