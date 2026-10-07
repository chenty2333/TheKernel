// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Repository MIT license.
//! Source-only oracle: temporary C carries MIT DRM and GPL-2.0-only chain
//! source grants. No upstream body, GPL implementation or hardware operation
//! is embedded in the runtime. This checks identity/terminal ordering, not GPU.
extern crate std;
use alloc::{format, string::String};
use std::{fs, path::PathBuf, process::Command};
fn function(source: &str, signature: &str) -> String {
    let start = source.find(signature).expect("upstream function changed");
    let begin = start + source[start..].find('{').unwrap();
    let mut depth = 0;
    for (offset, c) in source[begin..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[start..=begin + offset].into();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated function")
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
#[ignore = "requires local Linux7.2.3/GCC; source oracle only"]
fn manual_signal_and_chain_dependency_order_match_unmodified_linux_c() {
    let reference =
        PathBuf::from(std::env::var_os("THEKERNEL_LINUX_REFERENCE").expect("Linux reference"));
    let state = PathBuf::from(std::env::var_os("THEKERNEL_STATE_DIR").expect("state under /home"));
    assert!(state.is_absolute() && state.starts_with("/home"));
    let dir = Temporary(
        state
            .join("test-tmp")
            .join(format!("syncobj-c-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let drm = fs::read_to_string(reference.join("drivers/gpu/drm/drm_syncobj.c")).unwrap();
    let chain = fs::read_to_string(reference.join("drivers/dma-buf/dma-fence-chain.c")).unwrap();
    let legal = fs::read_to_string(reference.join("LICENSES/preferred/GPL-2.0")).unwrap();
    let functions = [
        function(&drm, "void drm_syncobj_replace_fence("),
        function(&drm, "static int drm_syncobj_assign_null_handle("),
        function(&chain, "void dma_fence_chain_init("),
        function(&chain, "int dma_fence_chain_find_seqno("),
        function(&chain, "static bool dma_fence_chain_signaled("),
    ]
    .join("\n");
    let prefix = r#"
#include <stdint.h>
#include <stdbool.h>
#include <stdio.h>
#include <stddef.h>
#define ENOMEM 12
#define EINVAL 22
#define spin_lock(p) ((void)0)
#define spin_unlock(p) ((void)0)
#define rcu_assign_pointer(p,v) ((p)=(v))
#define rcu_dereference_protected(p,c) (p)
#define list_for_each_entry_safe(p,t,h,n) for((p)=NULL;(p)!=NULL;(p)=NULL)
#define syncobj_wait_syncobj_func(s,w) ((void)0)
#define syncobj_eventfd_entry_func(s,w) ((void)0)
#define ktime_get() 0
#define lockdep_set_class(p,k) ((void)0)
#define max(a,b) ((a)>(b)?(a):(b))
#define WARN_ON(x) ((x)?(__builtin_trap(),1):0)
#define dma_fence_is_chain(f) ((f)->chain)
struct lock_class_key {int unused;};
struct dma_fence {uint64_t context,seqno;bool done,chain;int inline_lock;};
struct dma_fence_chain {struct dma_fence base;struct dma_fence*prev,*fence;uint64_t prev_seqno;};
struct drm_syncobj {struct dma_fence*fence;int lock,cb_list,ev_fd_list;};
struct syncobj_wait_entry {int unused;};struct syncobj_eventfd_entry {int unused;};
static int dma_fence_chain_ops;
static struct dma_fence*dma_fence_get(struct dma_fence*f){return f;}
static void dma_fence_put(struct dma_fence*f){}
static struct dma_fence*dma_fence_allocate_private_stub(int t){static struct dma_fence stub={.done=true};return &stub;}
static struct dma_fence_chain*to_dma_fence_chain(struct dma_fence*f){return f && f->chain?(struct dma_fence_chain*)f:NULL;}
static bool __dma_fence_is_later(struct dma_fence*f,uint64_t a,uint64_t b){return a>b;}
static uint64_t dma_fence_context_alloc(int n){static uint64_t c=0;return ++c;}
static void dma_fence_init64(struct dma_fence*f,void*ops,void*lock,uint64_t c,uint64_t s){f->context=c;f->seqno=s;f->chain=true;f->done=false;}
static struct dma_fence*dma_fence_chain_walk(struct dma_fence*f){struct dma_fence_chain*c=to_dma_fence_chain(f);return c?c->prev:NULL;}
#define dma_fence_chain_for_each(i,h) for((i)=dma_fence_get(h);(i);(i)=dma_fence_chain_walk(i))
static struct dma_fence*dma_fence_chain_contained(struct dma_fence*f){struct dma_fence_chain*c=to_dma_fence_chain(f);return c?c->fence:f;}
static bool dma_fence_chain_signaled(struct dma_fence*f);
static bool dma_fence_is_signaled(struct dma_fence*f){return f->chain?dma_fence_chain_signaled(f):f->done;}
static int ready(struct dma_fence*f,uint64_t point){dma_fence_chain_find_seqno(&f,point);return dma_fence_is_signaled(f);}
"#;
    // ready() needs the source prototype before the unchanged function bodies.
    let prefix = prefix.replace(
        "static int ready(",
        "int dma_fence_chain_find_seqno(struct dma_fence**,uint64_t);\nstatic int ready(",
    );
    let main = r#"
int main(void){struct dma_fence a={0};struct drm_syncobj s={.fence=&a};drm_syncobj_assign_null_handle(&s);printf("B %d %d %d\n",a.done,s.fence->done,s.fence!=&a);
struct dma_fence_chain c4={0},c5={0};struct dma_fence stub={.done=true};dma_fence_chain_init(&c4,NULL,&a,4);dma_fence_chain_init(&c5,&c4.base,&stub,5);
printf("T %d %d %d\n",ready(&c5.base,3),ready(&c5.base,4),ready(&c5.base,5));a.done=true;printf("D %d %d\n",ready(&c5.base,4),ready(&c5.base,5));
struct dma_fence x={0},y={0};struct dma_fence_chain c7={0},c6={0};dma_fence_chain_init(&c7,NULL,&x,7);dma_fence_chain_init(&c6,&c7.base,&y,6);x.done=true;printf("N %llu %d %d\n",(unsigned long long)c6.base.seqno,ready(&c6.base,6),ready(&c6.base,7));y.done=true;printf("E %d %d\n",ready(&c6.base,6),ready(&c6.base,7));
struct dma_fence job={0};struct dma_fence_chain old={0},late={0};dma_fence_chain_init(&old,NULL,&stub,5);dma_fence_chain_init(&late,&old.base,&job,4);printf("L %d %d %d\n",job.done,ready(&late.base,5),ready(&old.base,5));}
"#;
    let copyright = &drm[..drm.find("*/").unwrap() + 2];
    let source = dir.0.join("oracle.c");
    fs::write(
        &source,
        format!(
            "/* SPDX-License-Identifier: GPL-2.0-only\nCopyright (C) 2018 Advanced Micro Devices, \
             Inc.\n{legal}\n*/\n{copyright}\n{prefix}\n{functions}\n{main}"
        ),
    )
    .unwrap();
    let executable = dir.0.join("oracle");
    let build = Command::new("gcc")
        .args(["-std=gnu11", "-O2", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let output = Command::new(executable).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "B 0 1 1\nT 0 0 0\nD 1 1\nN 7 0 0\nE 1 1\nL 0 0 1\n"
    );
}
