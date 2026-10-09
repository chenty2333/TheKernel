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
static int open_usb_keyboard(char *physical, size_t size) {
    for (int i=0;i<32;i++) {
        char path[64]; snprintf(path,sizeof path,"/dev/input/event%d",i);
        int fd=open(path,O_RDONLY|O_NONBLOCK|O_CLOEXEC); if(fd<0) continue;
        struct input_id id; unsigned char bits[128]={0};
        if(ioctl(fd,EVIOCGID,&id)<0 || id.bustype!=BUS_USB ||
           ioctl(fd,EVIOCGBIT(EV_KEY,sizeof bits),bits)<0 || !bit(bits,KEY_A) ||
           ioctl(fd,EVIOCGPHYS(size),physical)<0 || !physical[0]) {
            close(fd); continue;
        }
        physical[size-1]=0;
        return fd;
    }
    return -1;
}
static int removed_keyboard(int fd) {
    struct pollfd p={fd,POLLIN,0};
    if(poll(&p,1,0)!=1 || !(p.revents&(POLLHUP|POLLERR))) return 0;
    struct input_event event;
    return read(fd,&event,sizeof event)==-1 && errno==ENODEV;
}
static int hotplug(void) {
    char original[256]={0}, physical[256]={0};
    int old=open_usb_keyboard(original,sizeof original), fresh=-1, failed=1;
    if(old<0) { fputs("hotplug: initial USB keyboard/physical identity missing\n",stderr); return 1; }
    printf("N305_USB_HOTPLUG_IDENTITY physical=%s\n",original);
    puts("\nN305_USB_HOTPLUG_UNPLUG_WAIT"); fflush(stdout);
    time_t end=time(NULL)+20;
    while(time(NULL)<end && !removed_keyboard(old)) usleep(100000);
    if(!removed_keyboard(old)) { fputs("hotplug: old keyboard did not revoke\n",stderr); goto out; }
    puts("\nN305_USB_HOTPLUG_REPLUG_WAIT"); fflush(stdout);
    end=time(NULL)+20;
    while(time(NULL)<end) {
        fresh=open_usb_keyboard(physical,sizeof physical);
        if(fresh>=0) break;
        usleep(100000);
    }
    if(fresh<0 || strcmp(original,physical)!=0) {
        fprintf(stderr,"hotplug: replacement keyboard/route mismatch old=%s new=%s\n",original,physical); goto out;
    }
    if(!removed_keyboard(old)) { fputs("hotplug: old fd revived after replug\n",stderr); goto out; }
    puts("\nN305_USB_HOTPLUG_KEY_WAIT"); fflush(stdout);
    end=time(NULL)+15;
    int key=0;
    while(time(NULL)<end && !key) {
        struct pollfd p={fresh,POLLIN,0};
        if(poll(&p,1,100)<0 && errno!=EINTR) goto out;
        struct input_event events[16]; ssize_t bytes=read(fresh,events,sizeof events);
        if(bytes<0) { if(errno==EAGAIN || errno==EINTR) continue; goto out; }
        for(size_t i=0;i<(size_t)bytes/sizeof events[0];i++)
            if(events[i].type==EV_KEY && events[i].code==KEY_A && events[i].value==1) key=1;
    }
    if(!key || !removed_keyboard(old)) { fputs("hotplug: replacement event/old-fd revocation failed\n",stderr); goto out; }
    failed=0;
    puts("\nN305_USB_HOTPLUG_PASS old_fd=revoked route=stable new_keyboard=verified"); fflush(stdout);
out:
    if(fresh>=0) close(fresh);
    close(old);
    return failed;
}
int main(int argc,char **argv) {
    if(argc==2 && strcmp(argv[1],"--hotplug")==0) return hotplug();
    if(argc!=1) { fputs("usage: usb-input-smoke [--hotplug]\n",stderr); return 2; }
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
                if(events[j].type==EV_REL && events[j].code==REL_X && events[j].value==17) rel=1;
                if(events[j].type==EV_ABS && events[j].code==ABS_X && events[j].value==16384) abs=1;
            }
        }
    }
    for(int i=0;i<count;i++) close(files[i].fd);
    if(!(key&&rel&&abs)) { fprintf(stderr,"USB events missing: key=%d rel=%d abs=%d\n",key,rel,abs); return 1; }
    puts("\nN305_USB_INPUT_PASS keyboard relative absolute"); fflush(stdout); return 0;
}
