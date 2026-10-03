/* Known stereo S16LE/48000 waveform through the existing OSS endpoint. */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>
static int16_t samples[16384];
static void check(int ok,const char *what){if(!ok){perror(what);exit(1);}}
int main(void){
    for(int frame=0;frame<8192;++frame){int value=((frame*97)%20001)-10000;samples[frame*2]=value;samples[frame*2+1]=-value;}
    int fd=open("/dev/dsp",O_WRONLY);check(fd>=0,"open dsp");
    int rate=48000,format=16,channels=2;
    check(ioctl(fd,0xc0045002,&rate)==0 && rate==48000,"rate");
    check(ioctl(fd,0xc0045005,&format)==0 && format==16,"format");
    check(ioctl(fd,0xc0045006,&channels)==0 && channels==2,"channels");
    const unsigned char *p=(const unsigned char *)samples;int done=0;
    while(done<(int)sizeof(samples)){int n=write(fd,p+done,sizeof(samples)-done);check(n>0,"write waveform");done+=n;}
    check(ioctl(fd,0x5001,0)==0,"drain waveform");check(close(fd)==0,"close");
    usleep(100000);puts("HDA_WAVEFORM_SUBMITTED");return 0;
}
