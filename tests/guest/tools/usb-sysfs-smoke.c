#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

struct device {unsigned bus,address,vendor,product;};
static void need(int ok,const char *what) {if(!ok){fprintf(stderr,"USB sysfs: %s errno=%d (%s)\n",what,errno,strerror(errno));exit(1);}}
static unsigned attribute(const char *name,const char *field,int hex) {
    char path[512];snprintf(path,sizeof(path),"/sys/bus/usb/devices/%s/%s",name,field);
    FILE *file=fopen(path,"r");need(file!=NULL,"attribute open");unsigned value;char newline;
    need(fscanf(file,hex?"%x%c":"%u%c",&value,&newline)==2 && newline=='\n' && fgetc(file)==EOF,"attribute format");fclose(file);return value;
}
int main(int argc,char **argv) {
    int tools=argc==2 && !strcmp(argv[1],"--tools");need(argc==1 || tools,"arguments");
    struct device devices[32];unsigned count=0;
    DIR *dir=opendir("/sys/bus/usb/devices");need(dir!=NULL,"real inventory directory");struct dirent *entry;
    while((entry=readdir(dir))) {
        if(entry->d_name[0]=='.' || strchr(entry->d_name,':'))continue;
        need(count<32,"test inventory bound");struct device *device=&devices[count];
        device->bus=attribute(entry->d_name,"busnum",0);device->address=attribute(entry->d_name,"devnum",0);
        device->vendor=attribute(entry->d_name,"idVendor",1);device->product=attribute(entry->d_name,"idProduct",1);
        need(device->bus>0 && device->bus<=255 && device->address>0 && device->address<=127,"real USB address bounds");
        for(unsigned i=0;i<count;i++)need(devices[i].bus!=device->bus || devices[i].address!=device->address,"unique addressed device");
        char path[512];snprintf(path,sizeof(path),"/sys/bus/usb/devices/%s/descriptors",entry->d_name);
        int fd=open(path,O_RDONLY|O_CLOEXEC);need(fd>=0,"cached descriptors open");unsigned char bytes[65536];size_t used=0;ssize_t size;
        while(used<sizeof(bytes) && (size=read(fd,bytes+used,sizeof(bytes)-used))>0)used+=size;
        need(used>=18 && used<sizeof(bytes) && bytes[0]==18 && bytes[1]==1,"standard device descriptor");close(fd);
        need((unsigned)(bytes[8]|bytes[9]<<8)==device->vendor && (unsigned)(bytes[10]|bytes[11]<<8)==device->product,"real descriptor/attribute identity");
        unsigned configs=0;size_t offset=18;
        while(offset<used) {need(used-offset>=9 && bytes[offset]>=9 && bytes[offset+1]==2,"raw configuration header");
            size_t length=bytes[offset+2]|bytes[offset+3]<<8;need(length>=9 && length<=used-offset,"raw configuration bound");offset+=length;configs++;}
        need(configs==bytes[17] && configs==attribute(entry->d_name,"bNumConfigurations",0),"all cached configurations retained");
        printf("USB_DEVICE bus=%u address=%u id=%04x:%04x node=%s bytes=%zu\n",device->bus,device->address,device->vendor,device->product,entry->d_name,used);count++;
    }
    closedir(dir);need(count>=3,"QEMU's actual keyboard/mouse/tablet enumeration");
    if(tools) {
        int output[2];need(!pipe(output),"lsusb pipe");pid_t child=fork();need(child>=0,"lsusb fork");
        if(!child){close(output[0]);dup2(output[1],STDOUT_FILENO);dup2(output[1],STDERR_FILENO);close(output[1]);execl("/opt/thekernel-tools/bin/lsusb","lsusb",(char *)NULL);_exit(127);}
        close(output[1]);char text[32768];size_t used=0;ssize_t size;
        while(used<sizeof(text)-1 && (size=read(output[0],text+used,sizeof(text)-1-used))>0)used+=size;
        need(used<sizeof(text)-1,"lsusb output bound");text[used]=0;close(output[0]);int status;
        need(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"real signed lsusb exit");printf("USB_LSUSB\n%s",text);
        for(unsigned i=0;i<count;i++){char expected[128];snprintf(expected,sizeof(expected),"Bus %03u Device %03u: ID %04x:%04x",devices[i].bus,devices[i].address,devices[i].vendor,devices[i].product);need(strstr(text,expected)!=NULL,"real lsusb lists every observed device");}
    }
    puts("KTAP version 1\n1..1\nok 1 - actual USB sysfs inventory and cached descriptors");return 0;
}
