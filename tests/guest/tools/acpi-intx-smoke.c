#define _DEFAULT_SOURCE
/* Original TheKernel INTx TCP wire-data regression, Apache-2.0. */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <arpa/inet.h>
#include <fcntl.h>
static void check(int ok,const char *s){if(!ok){perror(s);exit(1);}}
static unsigned long long irqs(unsigned vector){
 FILE *f=fopen("/proc/interrupts","r");check(f!=NULL,"interrupts");char line[2048];unsigned long long total=0;
 while(fgets(line,sizeof(line),f)){unsigned n;char *p=strchr(line,':');if(!p||sscanf(line," %u:",&n)!=1||n!=vector)continue;
  char *end;for(p++; ;p=end){unsigned long long v=strtoull(p,&end,10);if(p==end)break;total+=v;}
 }
 fclose(f);return total;
}
static void only_intx(void){
 unsigned char config[256];int fd=open("/sys/bus/pci/devices/0000:00:06.0/config",O_RDONLY);check(fd>=0,"NIC config");
 check(read(fd,config,sizeof(config))==sizeof(config),"NIC config read");close(fd);
 check(config[0]==0xf4&&config[1]==0x1a,"tested VirtIO NIC identity");
 unsigned offset=config[0x34],budget=48;
 while(offset){check(budget--&&offset>=0x40&&offset<255&&(offset&3)==0,"capability chain");
  check(config[offset]!=5&&config[offset]!=0x11,"MSI and MSI-X must be absent");offset=config[offset+1];}
 check(config[0x3d]==1,"NIC INTA pin");puts("THEKERNEL_ACPI_NIC_INTX_ONLY");
}
int main(int argc,char **argv){
 check(argc==3,"usage PORT VECTOR");only_intx();unsigned vector=strtoul(argv[2],NULL,10);unsigned long long before=irqs(vector);
 int fd=socket(AF_INET,SOCK_STREAM,0);check(fd>=0,"socket");struct timeval tv={5,0};setsockopt(fd,SOL_SOCKET,SO_RCVTIMEO,&tv,sizeof(tv));setsockopt(fd,SOL_SOCKET,SO_SNDTIMEO,&tv,sizeof(tv));
 struct sockaddr_in a={.sin_family=AF_INET,.sin_port=htons(atoi(argv[1]))};inet_pton(AF_INET,"10.0.2.2",&a.sin_addr);check(connect(fd,(void *)&a,sizeof(a))==0,"external TCP connect");
 unsigned char out[1024],in[1024];size_t bytes=0;
 for(unsigned round=0;round<64;round++){for(unsigned i=0;i<sizeof(out);i++)out[i]=(unsigned char)(i*31+round*17);
  size_t off=0;while(off<sizeof(out)){ssize_t n=write(fd,out+off,sizeof(out)-off);check(n>0,"wire write");off+=n;}
  off=0;while(off<sizeof(in)){ssize_t n=read(fd,in+off,sizeof(in)-off);check(n>0,"wire read");off+=n;}
  check(memcmp(out,in,sizeof(out))==0,"wire byte comparison");bytes+=sizeof(out);usleep(10000);
 }
 close(fd);usleep(50000);unsigned long long after=irqs(vector);check(after>before,"INTx counter increased");
 printf("THEKERNEL_ACPI_INTX_IO_OK bytes=%zu vector=%u before=%llu after=%llu\n",bytes,vector,before,after);return 0;
}
