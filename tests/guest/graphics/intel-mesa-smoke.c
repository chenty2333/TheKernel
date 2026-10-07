/* SPDX-License-Identifier: MIT
 * Copyright 2026 TheKernel contributors.
 * Explicit real Mesa/iris acceptance. Never invoked by default guest tests.
 * Initialization and shader/result markers are separate, neither is a model.
 */
#define _GNU_SOURCE
#include <drm.h>
#include <i915_drm.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static GLuint shader(GLenum type, const char *text) {
    GLuint object=glCreateShader(type);
    glShaderSource(object,1,&text,NULL);glCompileShader(object);
    GLint okay=0;glGetShaderiv(object,GL_COMPILE_STATUS,&okay);
    if(!okay){char log[2048]={0};glGetShaderInfoLog(object,sizeof(log),NULL,log);fprintf(stderr,"INTEL_MESA_FAIL shader %s\n",log);glDeleteShader(object);return 0;}
    return object;
}
int main(int argc,char **argv) {
    if(argc!=3 || (strcmp(argv[1],"--initialize") && strcmp(argv[1],"--execute"))) {
        fprintf(stderr,"usage: intel-mesa-smoke {--initialize|--execute} /dev/dri/renderD128\n");return 2;
    }
    int result=1,fd=-1,chipset=0;
    struct gbm_device *gbm=NULL;
    EGLDisplay display=EGL_NO_DISPLAY;EGLContext context=EGL_NO_CONTEXT;
    GLuint vertex=0,fragment=0,program=0,texture=0,framebuffer=0;
    /* Do not permit llvmpipe/virgl fallback to produce a success marker. */
    if(setenv("MESA_LOADER_DRIVER_OVERRIDE","iris",1)||unsetenv("LIBGL_ALWAYS_SOFTWARE")){perror("environment");return 1;}
    fd=open(argv[2],O_RDWR|O_CLOEXEC);if(fd<0){perror("DRM open; not tested");goto done;}
    drm_i915_getparam_t query={.param=I915_PARAM_CHIPSET_ID,.value=&chipset};
    if(ioctl(fd,DRM_IOCTL_I915_GETPARAM,&query)||chipset!=0x46d0){fprintf(stderr,"INTEL_MESA_FAIL not N305 i915\n");goto done;}
    gbm=gbm_create_device(fd);if(!gbm){fprintf(stderr,"INTEL_MESA_FAIL GBM\n");goto done;}
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display=(PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if(!get_display){fprintf(stderr,"INTEL_MESA_FAIL platform entry\n");goto done;}
    display=get_display(EGL_PLATFORM_GBM_KHR,gbm,NULL);
    EGLint major=0,minor=0;
    if(display==EGL_NO_DISPLAY || !eglInitialize(display,&major,&minor)){fprintf(stderr,"INTEL_MESA_FAIL initialize error=0x%x\n",eglGetError());goto done;}
    if(!eglBindAPI(EGL_OPENGL_ES_API))goto done;
    EGLint attributes[]={EGL_RENDERABLE_TYPE,EGL_OPENGL_ES3_BIT_KHR,EGL_NONE};
    EGLConfig config;EGLint count=0;
    if(!eglChooseConfig(display,attributes,&config,1,&count)||count!=1)goto done;
    EGLint context_attributes[]={EGL_CONTEXT_CLIENT_VERSION,3,EGL_NONE};
    context=eglCreateContext(display,config,EGL_NO_CONTEXT,context_attributes);
    if(context==EGL_NO_CONTEXT || !eglMakeCurrent(display,EGL_NO_SURFACE,EGL_NO_SURFACE,context)){fprintf(stderr,"INTEL_MESA_FAIL context error=0x%x\n",eglGetError());goto done;}
    const char *renderer=(const char*)glGetString(GL_RENDERER);
    if(!renderer || !strstr(renderer,"Intel") || strstr(renderer,"llvmpipe") || strstr(renderer,"softpipe") || strstr(renderer,"virgl") || strstr(renderer,"zink")){fprintf(stderr,"INTEL_MESA_FAIL renderer=%s\n",renderer?renderer:"null");goto done;}
    printf("INTEL_MESA_INITIALIZED egl=%d.%d renderer=%s; not shader/result acceptance\n",major,minor,renderer);
    if(!strcmp(argv[1],"--initialize")){result=0;goto done;}
    const char *vs="#version 300 es\nconst vec2 positions[3]=vec2[3](vec2(-1.,-1.),vec2(1.,-1.),vec2(0.,1.));void main(){gl_Position=vec4(positions[gl_VertexID],0.,1.);}";
    const char *fs="#version 300 es\nprecision highp float;out vec4 color;void main(){color=vec4(0.2,0.6,0.8,1.0);}";
    vertex=shader(GL_VERTEX_SHADER,vs);fragment=shader(GL_FRAGMENT_SHADER,fs);if(!vertex||!fragment)goto done;
    program=glCreateProgram();glAttachShader(program,vertex);glAttachShader(program,fragment);glLinkProgram(program);
    GLint linked=0;glGetProgramiv(program,GL_LINK_STATUS,&linked);if(!linked){fprintf(stderr,"INTEL_MESA_FAIL link\n");goto done;}
    glGenTextures(1,&texture);glBindTexture(GL_TEXTURE_2D,texture);glTexStorage2D(GL_TEXTURE_2D,1,GL_RGBA8,64,64);
    glGenFramebuffers(1,&framebuffer);glBindFramebuffer(GL_FRAMEBUFFER,framebuffer);glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,texture,0);
    if(glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE)goto done;
    glViewport(0,0,64,64);glDisable(GL_DITHER);glClearColor(0.,0.,0.,1.);glClear(GL_COLOR_BUFFER_BIT);glUseProgram(program);glDrawArrays(GL_TRIANGLES,0,3);glFinish();
    unsigned char pixels[64*64*4];glReadPixels(0,0,64,64,GL_RGBA,GL_UNSIGNED_BYTE,pixels);
    GLenum error=glGetError();if(error!=GL_NO_ERROR){fprintf(stderr,"INTEL_MESA_FAIL GL error=0x%x\n",error);goto done;}
    /* Check large interior/exterior regions, excluding rasterization edges. */
    unsigned inside=0,outside=0;
    for(unsigned y=0;y<64;y++)for(unsigned x=0;x<64;x++) {
        int kind=(y>=16&&y<32&&x>=24&&x<40)?1:((y>=48&&x<8)?2:0);if(!kind)continue;
        const unsigned char *p=&pixels[(y*64+x)*4];
        const unsigned char expected[4]={kind==1?51:0,kind==1?153:0,kind==1?204:0,255};
        if(memcmp(p,expected,4)){fprintf(stderr,"INTEL_MESA_FAIL pixel=%u,%u rgba=%u,%u,%u,%u\n",x,y,p[0],p[1],p[2],p[3]);goto done;}
        if(kind==1)inside++;else outside++;
    }
    printf("INTEL_MESA_SHADER_PIXELS_VERIFIED inside=%u outside=%u; real client, separate from minimal RCS selftest\n",inside,outside);result=0;
done:
    if(context!=EGL_NO_CONTEXT){if(framebuffer)glDeleteFramebuffers(1,&framebuffer);if(texture)glDeleteTextures(1,&texture);if(program)glDeleteProgram(program);if(vertex)glDeleteShader(vertex);if(fragment)glDeleteShader(fragment);eglMakeCurrent(display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);eglDestroyContext(display,context);}
    if(display!=EGL_NO_DISPLAY)eglTerminate(display);
    if(gbm)gbm_device_destroy(gbm);
    if(fd>=0)close(fd);
    return result;
}
