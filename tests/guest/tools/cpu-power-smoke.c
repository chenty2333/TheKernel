#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define ROOT "/sys/devices/system/cpu"
static int text(const char *path,char *buffer,size_t len) {
    int fd=open(path,O_RDONLY); if(fd<0) return -1;
    ssize_t got=read(fd,buffer,len-1); close(fd); if(got<0) return -1; buffer[got]=0; return 0;
}
static unsigned long long number(const char *path) {
    char buf[128]; if(text(path,buf,sizeof(buf))) return ~0ULL; return strtoull(buf,NULL,10);
}
static int idle(void) {
    unsigned long long before[64][6][2]={0}; int counts[64]={0}, supported[64]={0}, cpus=0;
    char path[256],buf[128]; int enabled=number(ROOT "/cpuidle/mwait_enabled")==1;
    for(int cpu=0;cpu<64;cpu++) {
        snprintf(path,sizeof(path),ROOT "/cpu%d/online",cpu);
        if(number(path)!=1) break;
        cpus++;
        snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/mwait_supported",cpu); supported[cpu]=number(path)==1;
        for(int state=0;state<6;state++) {
            snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/name",cpu,state);
            if(text(path,buf,sizeof(buf))) break;
            if(!buf[0]) return 1;
            counts[cpu]++;
            for(int field=0;field<2;field++) {
                snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/%s",cpu,state,field?"time":"usage");
                before[cpu][state][field]=number(path); if(before[cpu][state][field]==~0ULL) return 1;
            }
        }
        if(!counts[cpu]) return 1;
    }
    if(!cpus) return 1;
    usleep(300000);
    for(int cpu=0;cpu<cpus;cpu++) {
        unsigned long long usage=0,time=0;
        for(int state=enabled&&supported[cpu]?1:0;state<counts[cpu];state++) {
            snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/usage",cpu,state);
            usage+=number(path)-before[cpu][state][0];
            snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/time",cpu,state);
            time+=number(path)-before[cpu][state][1];
        }
        printf("CPU_IDLE cpu=%d mwait_enabled=%d supported=%d usage_delta=%llu time_us_delta=%llu\n",cpu,enabled,supported[cpu],usage,time);
        if(!usage||!time) return 1;
    }
    snprintf(path,sizeof(path),ROOT "/cpu0/cpuidle/state0/disable");
    int fd=open(path,O_WRONLY); if(fd<0) return 1;
    errno=0; int result=write(fd,"2\n",2); close(fd); if(result!=-1||errno!=EINVAL) return 1;
    puts("THEKERNEL_CPU_IDLE_OK");return 0;
}
static int frequency(void) {
    char buf[256],path[256];
    unsigned long long supported=number(ROOT "/cpu0/cpufreq_supported");
    if(supported==~0ULL) return 1;
    if(!supported) {
        errno=0; int fd=open(ROOT "/cpu0/cpufreq/scaling_driver",O_RDONLY);
        if(fd>=0||errno!=ENOENT) { if(fd>=0) close(fd); return 1; }
        puts("CPU_FREQUENCY unsupported (no firmware-enabled HWP frequency reference)"); return 0;
    }
    if(text(ROOT "/cpu0/cpufreq/scaling_driver",buf,sizeof(buf))||strcmp(buf,"intel_pstate\n")) return 1;
    const char *fields[]={"cpuinfo_min_freq","scaling_min_freq","scaling_max_freq","cpuinfo_max_freq"};
    unsigned long long previous=0;
    for(unsigned i=0;i<4;i++) { snprintf(path,sizeof(path),ROOT "/cpu0/cpufreq/%s",fields[i]); unsigned long long value=number(path); if(value==~0ULL||value<previous) return 1; previous=value; }
    puts("CPU_FREQUENCY supported"); return 0;
}
int main(void) { return idle()||frequency(); }
