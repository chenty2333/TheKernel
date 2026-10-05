#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <linux/nsfs.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int good,const char *name) {
    if(!good){fprintf(stderr,"namespace-relationships: %s errno=%d (%s)\n",name,errno,strerror(errno));exit(1);}
}
static ino_t inode(int fd) {struct stat st;require(!fstat(fd,&st),"namespace fstat");return st.st_ino;}
static int nsopen(const char *type) {
    char path[96];snprintf(path,sizeof(path),"/proc/self/ns/%s",type);
    int fd=open(path,O_RDONLY|O_CLOEXEC);require(fd>=0,"namespace open");return fd;
}
static void denied(int fd,unsigned command,const char *name) {
    errno=0;int result=ioctl(fd,command);
    if(result>=0)close(result);
    require(result==-1 && errno==EPERM,name);
}
static void parent_matches(int fd,unsigned command,ino_t expected) {
    int parent=ioctl(fd,command);require(parent>=0,"visible user namespace relationship");
    require(fcntl(parent,F_GETFD)==FD_CLOEXEC,"related namespace descriptor closes on exec");
    require(inode(parent)==expected && ioctl(parent,NS_GET_NSTYPE)==CLONE_NEWUSER,"returned parent identity/type");close(parent);
}
static void real_lsns(void) {
    int pipefd[2];require(!pipe2(pipefd,O_CLOEXEC),"lsns pipe");pid_t child=fork();require(child>=0,"lsns fork");
    if(!child){close(pipefd[0]);dup2(pipefd[1],STDOUT_FILENO);close(pipefd[1]);
        execl("/opt/thekernel-tools/bin/lsns","lsns",(char *)NULL);_exit(127);}
    close(pipefd[1]);char text[16384];size_t length=0;ssize_t n;
    while(length<sizeof(text)-1 && (n=read(pipefd[0],text+length,sizeof(text)-1-length))>0)length+=n;
    text[length]=0;close(pipefd[0]);int status;
    require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"real lsns exit");
    printf("%s",text);require(strstr(text,"mnt") && strstr(text,"net") && strstr(text,"user"),"real lsns namespace rows");
}
static void nested_relationships(int olduser,int oldnet,int tools) {
    ino_t parent_inode=inode(olduser);int report[2],release[2];
    require(!pipe2(report,O_CLOEXEC) && !pipe2(release,O_CLOEXEC),"namespace fixture pipes");
    pid_t child=fork();require(child>=0,"namespace fixture fork");
    if(!child) {
        close(report[0]);close(release[1]);require(!unshare(CLONE_NEWUSER),"new user namespace");
        denied(oldnet,NS_GET_USERNS,"inherited outer-owned endpoint cannot expose ancestor user namespace");
        int own=nsopen("user");denied(own,NS_GET_PARENT,"child cannot expose its own parent");
        denied(own,NS_GET_USERNS,"child cannot expose its own owning parent");ino_t value=inode(own);
        require(write(report[1],&value,sizeof(value))==sizeof(value),"report child namespace identity");
        char token;require(read(release[0],&token,1)==1,"release child namespace fixture");
        close(own);close(report[1]);close(release[0]);_exit(0);
    }
    close(report[1]);close(release[0]);ino_t child_inode;
    require(read(report[0],&child_inode,sizeof(child_inode))==sizeof(child_inode),"receive child namespace identity");close(report[0]);
    char path[96];snprintf(path,sizeof(path),"/proc/%ld/ns/user",(long)child);
    int child_ns=open(path,O_RDONLY|O_CLOEXEC);require(child_ns>=0 && inode(child_ns)==child_inode,"parent opens real child namespace");
    parent_matches(child_ns,NS_GET_PARENT,parent_inode);parent_matches(child_ns,NS_GET_USERNS,parent_inode);
    pid_t no_admin=fork();require(no_admin>=0,"no-admin relationship fork");
    if(!no_admin) {
        struct __user_cap_header_struct header={.version=_LINUX_CAPABILITY_VERSION_3};
        struct __user_cap_data_struct data[2];require(!syscall(SYS_capget,&header,data),"read capabilities");
        data[0].effective&=~(1U<<CAP_SYS_ADMIN);data[0].permitted&=~(1U<<CAP_SYS_ADMIN);data[0].inheritable&=~(1U<<CAP_SYS_ADMIN);
        require(!syscall(SYS_capset,&header,data),"drop SYS_ADMIN");
        parent_matches(child_ns,NS_GET_PARENT,parent_inode);parent_matches(child_ns,NS_GET_USERNS,parent_inode);_exit(0);
    }
    int status;require(waitpid(no_admin,&status,0)==no_admin && WIFEXITED(status) && !WEXITSTATUS(status),"ancestry visibility does not require SYS_ADMIN");
    if(tools)real_lsns();
    require(write(release[1],"x",1)==1,"release nested child");close(release[1]);close(child_ns);
    require(waitpid(child,&status,0)==child && WIFEXITED(status) && !WEXITSTATUS(status),"nested child clean exit");
}
int main(int argc,char **argv) {
    alarm(30);int user=nsopen("user"),net=nsopen("net");
    denied(user,NS_GET_PARENT,"self user namespace parent is outside caller scope, not EINVAL");
    denied(user,NS_GET_USERNS,"self user namespace owner is outside caller scope");
    int mnt=nsopen("mnt");errno=0;require(ioctl(mnt,NS_GET_PARENT)==-1 && errno==EINVAL,"nonhierarchical namespace parent remains EINVAL");close(mnt);
    int owner=ioctl(net,NS_GET_USERNS);
    require(owner>=0 || errno==EPERM,"network namespace owner query shape");
    if(owner>=0){require(ioctl(owner,NS_GET_NSTYPE)==CLONE_NEWUSER && fcntl(owner,F_GETFD)==FD_CLOEXEC,"network namespace owner type and close-on-exec");close(owner);}
    if(argc==2 && (!strcmp(argv[1],"--nested") || !strcmp(argv[1],"--tools")))nested_relationships(user,net,!strcmp(argv[1],"--tools"));
    close(user);close(net);puts("THEKERNEL_NAMESPACE_RELATIONSHIPS_OK");return 0;
}
