#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <time.h>
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
    char path[256],buf[4096];
    if(text("/proc/cmdline",buf,sizeof(buf))) return 1;
    int expected=1; char *save=NULL;
    for(char *word=strtok_r(buf," \t\r\n",&save);word;word=strtok_r(NULL," \t\r\n",&save)) {
        if(!strcmp(word,"cpuidle.mwait")) expected=0;
        if(!strncmp(word,"cpuidle.mwait=",14)) expected=!strcmp(word+14,"1");
    }
    unsigned long long policy=number(ROOT "/cpuidle/mwait_enabled");
    if(policy>1||policy!=(unsigned)expected) return 1;
    int enabled=(int)policy;
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
    int mwait_cpus=0,mwait_idle=0,hlt_cpus=0,hlt_idle=0;
    for(int cpu=0;cpu<cpus;cpu++) {
        unsigned long long usage[2]={0},time[2]={0};
        for(int state=0;state<counts[cpu];state++) {
            snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/usage",cpu,state);
            unsigned long long after=number(path); if(after==~0ULL||after<before[cpu][state][0]) return 1;
            usage[state!=0]+=after-before[cpu][state][0];
            snprintf(path,sizeof(path),ROOT "/cpu%d/cpuidle/state%d/time",cpu,state);
            after=number(path); if(after==~0ULL||after<before[cpu][state][1]) return 1;
            time[state!=0]+=after-before[cpu][state][1];
        }
        printf("CPU_IDLE cpu=%d mwait_enabled=%d supported=%d hlt_usage=%llu mwait_usage=%llu hlt_time_us=%llu mwait_time_us=%llu\n",cpu,enabled,supported[cpu],usage[0],usage[1],time[0],time[1]);
        /* A CPU may stay busy for the whole window under TCG, so require
           entries per policy class across CPUs, not on every CPU. */
        if(enabled&&supported[cpu]) { mwait_cpus++; if(usage[1]&&time[1]) mwait_idle++; }
        else { if(usage[1]) return 1; hlt_cpus++; if(usage[0]&&time[0]) hlt_idle++; }
    }
    if((mwait_cpus&&!mwait_idle)||(hlt_cpus&&!hlt_idle)) return 1;
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
/* Bounded correctness probes, not latency or power benchmarks. Workers are
   pinned to exercise each online CPU, and busy loops do not yield/syscall. */
static uint64_t now_ns(void) {
    struct timespec t; if(clock_gettime(CLOCK_MONOTONIC,&t)) return 0;
    return (uint64_t)t.tv_sec*1000000000+(uint64_t)t.tv_nsec;
}
static int pin_cpu(int cpu) {
    cpu_set_t set;CPU_ZERO(&set);CPU_SET(cpu,&set);
    return pthread_setaffinity_np(pthread_self(),sizeof(set),&set);
}
static _Atomic int command,ack,wake_error,busy_stop;
static int remote_cpu;
static int wait_word(_Atomic int *word,int old) {
    struct timespec timeout={2,0};
    long rc=syscall(SYS_futex,(int *)word,FUTEX_WAIT_PRIVATE,old,&timeout,0,0);
    return rc<0 && errno!=EAGAIN && errno!=EINTR;
}
static void wake_word(_Atomic int *word) { (void)syscall(SYS_futex,(int *)word,FUTEX_WAKE_PRIVATE,INT_MAX,0,0,0); }
static void *remote_worker(void *unused) {
    (void)unused;
    if(pin_cpu(remote_cpu)) {atomic_store(&wake_error,1);return NULL;}
    for(int seq=1;seq<=100;seq++) {
        while(atomic_load_explicit(&command,memory_order_acquire)<seq) {
            if(wait_word(&command,seq-1)) {atomic_store(&wake_error,1);return NULL;}
        }
        atomic_store_explicit(&ack,seq,memory_order_release);wake_word(&ack);
    }
    return NULL;
}
struct busy_worker { int cpu,seed,error; uint64_t chunks,sum; };
static void *busy_loop(void *arg) {
    struct busy_worker *w=arg; if(pin_cpu(w->cpu)) {w->error=1;return NULL;}
    while(!atomic_load_explicit(&busy_stop,memory_order_relaxed)) {
        volatile uint64_t sum=0;
        for(unsigned i=0;i<4096;i++) sum+=i+(unsigned)w->seed;
        w->sum+=sum;w->chunks++;
    }
    return NULL;
}
static int scheduling(int limit) {
    cpu_set_t allowed; if(sched_getaffinity(0,sizeof(allowed),&allowed)) return 1;
    int cpus[64],count=0;
    for(int cpu=0;cpu<CPU_SETSIZE&&count<limit;cpu++) if(CPU_ISSET(cpu,&allowed)) cpus[count++]=cpu;
    if(!count||pin_cpu(cpus[0])) return 1;
    remote_cpu=cpus[count>1?1:0];pthread_t remote;
    if(pthread_create(&remote,NULL,remote_worker,NULL)) return 1;
    for(int seq=1;seq<=100;seq++) {
        struct timespec t={0,1000000};
        int error;
        do {error=clock_nanosleep(CLOCK_MONOTONIC,0,&t,&t);} while(error==EINTR);
        if(error) return 1;
        atomic_store_explicit(&command,seq,memory_order_release);wake_word(&command);
        for(;;) {
            int observed=atomic_load_explicit(&ack,memory_order_acquire);
            if(observed>=seq) break;
            if(atomic_load(&wake_error)||wait_word(&ack,observed)) return 1;
        }
    }
    if(pthread_join(remote,NULL)||atomic_load(&wake_error)) return 1;
    /* Affinity migration followed by timer sleep tests every local APIC. */
    for(int i=0;i<count;i++) {
        if(pin_cpu(cpus[i])) return 1;
        uint64_t before=now_ns();struct timespec t={0,2000000};
        if(clock_nanosleep(CLOCK_MONOTONIC,0,&t,NULL)||now_ns()<before+2000000) return 1;
    }
    puts("THEKERNEL_CPU_IDLE_WAKE_OK");
    if(pin_cpu(cpus[0])) return 1;
    struct busy_worker workers[128]={0};pthread_t threads[128];int n=count*2;
    for(int i=0;i<n;i++) {
        workers[i].cpu=cpus[i%count];workers[i].seed=i+1;
        if(pthread_create(&threads[i],NULL,busy_loop,&workers[i])) return 1;
    }
    unsigned heartbeats=0;uint64_t end=now_ns()+1000000000;
    do {
        struct timespec t={0,1000000};
        if(clock_nanosleep(CLOCK_MONOTONIC,0,&t,NULL)) return 1;
        heartbeats++;
    } while(now_ns()<end);
    atomic_store(&busy_stop,1);
    for(int i=0;i<n;i++) {
        if(pthread_join(threads[i],NULL)||workers[i].error||!workers[i].chunks) return 1;
        uint64_t chunk=4096ULL*4095/2+4096ULL*(unsigned)workers[i].seed;
        if(workers[i].sum!=workers[i].chunks*chunk) return 1;
    }
    printf("THEKERNEL_CPU_SCHED_LOAD_OK cpus=%d workers=%d heartbeats=%u\n",count,n,heartbeats);
    return sched_setaffinity(0,sizeof(allowed),&allowed)!=0;
}
int main(int argc, char **argv) {
    if(argc==2 && !strcmp(argv[1],"--tools")) {
        /* All info nodes are public. Avoid cpupower's root-only attempt to
           load Linux's msr module; TheKernel does not have loadable modules. */
        if(setgid(65534)||setuid(65534)) return 1;
        execl("/bin/sh","sh","/opt/thekernel-tests/power-tools.sh",(char *)NULL);
        return 1;
    }
    if(argc==2&&!strcmp(argv[1],"--scheduling-only")) return scheduling(4);
    int fd=open("/sys/class/hwmon",O_RDONLY|O_DIRECTORY); if(fd<0) return 1; close(fd);
    puts("CPU_THERMAL hwmon class accessible");
    if(scheduling(64)||idle()||frequency()) return 1;
    puts("THEKERNEL_CPU_POWER_OK"); return 0;
}
