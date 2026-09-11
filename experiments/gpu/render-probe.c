// Build in the guest: cc render-probe.c -o render-probe -lEGL -lGLESv2 -lgbm
// Verify that the guest can render through a virtio GPU, not a CPU rasterizer.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <gbm.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <ctype.h>
#define REQUIRE(x) do { if (!(x)) { fprintf(stderr,"FAIL line %d: %s (EGL 0x%x)\n",__LINE__,#x,eglGetError()); return 1; } } while (0)
int main(void) {
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    REQUIRE(fd >= 0);
    struct gbm_device *gbm = gbm_create_device(fd);
    REQUIRE(gbm);
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    REQUIRE(platform);
    EGLDisplay display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    REQUIRE(eglInitialize(display, &major, &minor));
    REQUIRE(eglBindAPI(EGL_OPENGL_ES_API));
    EGLint attributes[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_SURFACE_TYPE, 0, EGL_NONE };
    EGLConfig config; EGLint count;
    REQUIRE(eglChooseConfig(display, attributes, &config, 1, &count) && count);
    EGLint ctx_attributes[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, ctx_attributes);
    REQUIRE(context != EGL_NO_CONTEXT);
    REQUIRE(eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context));
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    REQUIRE(renderer);
    printf("RENDERER=%s\nGL_VERSION=%s\nEGL=%d.%d\n",renderer,glGetString(GL_VERSION),major,minor);
    char lower[1024];
    REQUIRE(strlen(renderer)<sizeof(lower));
    for (size_t i=0; i<=strlen(renderer); i++) lower[i]=(char)tolower((unsigned char)renderer[i]);
    REQUIRE(strstr(lower,"virgl") && !strstr(lower,"llvmpipe") && !strstr(lower,"softpipe") && !strstr(lower,"swrast"));
    GLuint texture, framebuffer;
    glGenTextures(1,&texture); glBindTexture(GL_TEXTURE_2D,texture);
    glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,32,32,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);
    glGenFramebuffers(1,&framebuffer); glBindFramebuffer(GL_FRAMEBUFFER,framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,texture,0);
    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER)==GL_FRAMEBUFFER_COMPLETE);
    glViewport(0,0,32,32);
    struct timespec started, ended;
    REQUIRE(clock_gettime(CLOCK_MONOTONIC,&started)==0);
    unsigned char pixel[4]; double elapsed_ms=0;
    for (int frame=0; frame<20; frame++) {
        glClearColor(frame%2 ? .75f : .25f,.5f,.75f,1.f); glClear(GL_COLOR_BUFFER_BIT);
        glReadPixels(16,16,1,1,GL_RGBA,GL_UNSIGNED_BYTE,pixel);
        REQUIRE(glGetError()==GL_NO_ERROR);
        int red=frame%2 ? 191 : 64;
        REQUIRE(pixel[0]>=red-1 && pixel[0]<=red+1 && pixel[1]>=127 && pixel[1]<=129 && pixel[2]>=190 && pixel[2]<=192 && pixel[3]==255);
        REQUIRE(clock_gettime(CLOCK_MONOTONIC,&ended)==0);
        elapsed_ms=(ended.tv_sec-started.tv_sec)*1000.+(ended.tv_nsec-started.tv_nsec)/1000000.;
        REQUIRE(elapsed_ms<2000.);
    }
    printf("GPU_RENDER_OK frames=20 elapsed_ms=%.2f pixel=%u,%u,%u,%u\n",elapsed_ms,pixel[0],pixel[1],pixel[2],pixel[3]);
    glDeleteFramebuffers(1,&framebuffer); glDeleteTextures(1,&texture);
    eglMakeCurrent(display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
    eglDestroyContext(display,context); eglTerminate(display); gbm_device_destroy(gbm); close(fd);
    return 0;
}
