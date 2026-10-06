# Copyright2026 TheKernel contributors; MIT. Temporary source-only oracle.
import os,sys,tempfile
from pathlib import Path
import re,subprocess
root=Path(os.environ['THEKERNEL_IGT_REFERENCE'])
state=Path(os.environ['THEKERNEL_STATE_DIR'])
assert state.is_absolute() and state.is_relative_to('/home')
temporary=tempfile.TemporaryDirectory(prefix='igt-page-',dir=state/'test-tmp')
work=Path(temporary.name)
(work/'linux').mkdir()
(work/'linux/bitops.h').write_text('#include <stdint.h>\n#define BIT(n) (1U<<(n))\n#define BIT_ULL(n) (1ULL<<(n))\n#define GENMASK(h,l) ((UINT32_MAX>>(31-(h)))&(UINT32_MAX<<(l)))\n#define GENMASK_ULL(h,l) ((UINT64_MAX>>(63-(h)))&(UINT64_MAX<<(l)))\n#define REG_BIT(n) BIT(n)\n#define REG_GENMASK(h,l) GENMASK(h,l)\n#define REG_FIELD_PREP(m,v) (((v)<<__builtin_ctz(m))&(m))\n')
(work/'linux_scaffold.h').write_text('#include <stdint.h>\ntypedef uint32_t u32;typedef uint64_t u64;typedef int64_t s64;\n')
s=(root/'rendercopy_gen9.c').read_text()
prefix=r'''
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <drm/i915_drm.h>
#include "linux_scaffold.h"
#define sign_extend64(v,b) (((int64_t)((uint64_t)(v)<<(63-(b))))>>(63-(b)))
#define lower_32_bits(v) ((uint32_t)(v))
#define upper_32_bits(v) ((uint32_t)((v)>>32))
#define I915_DISPATCH_SECURE 1
#include "gen9_render.h"
#include "xe2_render.h"
#include "intel_reg.h"
#define igt_assert(x) assert(x)
#define igt_assert_lte(a,b) assert((a)<=(b))
#define HAS_4TILE(d) false
#define IS_METEORLAKE(d) false
#define I915_COMPRESSION_MEDIA 1
#define I915_COMPRESSION_RENDER 2
#define I915_TILING_64 3
#define I915_TILING_Yf 4
#define I915_TILING_Ys 5
#define I915_TILING_4 6
#define DIV_ROUND_UP(n,d) (((n)+(d)-1)/(d))
#define DEBUG_RENDERCPY 0
struct intel_bb{uint8_t batch[4096];uint32_t ptr,handle,devid;uint64_t batch_offset;int fd;};
struct intel_buf{int bpp,depth,tiling,compression,mocs_index;unsigned width,height;uint32_t handle;struct{uint64_t offset;uint32_t stride;}surface[2],ccs[2];struct{uint64_t offset;}addr;struct{uint64_t offset;bool disable;}cc;};
static int intel_gen(int devid){return 12;}
static int intel_get_drm_devid(int fd){return 0x46d0;}
static unsigned intel_buf_width(const struct intel_buf*b){return b->width;}
static unsigned intel_buf_height(const struct intel_buf*b){return b->height;}
static bool intel_buf_pxp(const struct intel_buf*b){return false;}
static void*intel_bb_ptr(struct intel_bb*b){assert(b->ptr<4096);return b->batch+b->ptr;}
static void intel_bb_ptr_set(struct intel_bb*b,unsigned offset){assert(offset<4096);b->ptr=offset;}
static void*intel_bb_ptr_align(struct intel_bb*b,unsigned align){b->ptr=(b->ptr+align-1)&~(align-1);return intel_bb_ptr(b);}
static uint32_t intel_bb_offset(struct intel_bb*b){return b->ptr;}
static void intel_bb_ptr_add(struct intel_bb*b,unsigned count){assert(b->ptr+count<=4096);b->ptr+=count;}
static uint32_t intel_bb_ptr_add_return_prev_offset(struct intel_bb*b,unsigned count){unsigned prev=b->ptr;intel_bb_ptr_add(b,count);return prev;}
static void intel_bb_out(struct intel_bb*b,uint32_t n){assert(b->ptr%4==0);memcpy(intel_bb_ptr(b),&n,4);intel_bb_ptr_add(b,4);}
static uint32_t intel_bb_copy_data(struct intel_bb*b,const void*p,size_t n,unsigned align){intel_bb_ptr_align(b,align);unsigned prev=b->ptr;memcpy(intel_bb_ptr(b),p,n);intel_bb_ptr_add(b,n);return prev;}
static uint64_t intel_bb_offset_reloc_with_delta(struct intel_bb*b,unsigned h,unsigned r,unsigned w,unsigned delta,unsigned off,uint64_t base){return base;}
static uint64_t intel_bb_offset_reloc(struct intel_bb*b,unsigned h,unsigned r,unsigned w,unsigned off,uint64_t base){return base;}
static void intel_bb_emit_reloc(struct intel_bb*b,unsigned h,unsigned r,unsigned w,unsigned delta,uint64_t base){uint64_t addr=base+delta;intel_bb_out(b,addr);intel_bb_out(b,addr>>32);}
static void intel_bb_add_intel_buf(struct intel_bb*b,struct intel_buf*buf,bool write){}
static void intel_bb_flush_render(struct intel_bb*b){}
static bool intel_bb_pxp_enabled(struct intel_bb*b){return false;}
static int intel_bb_pxp_apptype(struct intel_bb*b){abort();}
static int intel_bb_pxp_appid(struct intel_bb*b){abort();}
static void intel_bb_emit_bbe(struct intel_bb*b){intel_bb_out(b,0x05000000);}
static void intel_bb_exec(struct intel_bb*b,unsigned end,unsigned flags,bool sync){assert(end<=2048);}
static void intel_bb_reset(struct intel_bb*b,bool purge){}
static void dump_batch(struct intel_bb*b){}
#define VERTEX_SIZE (3*4)
#define BATCH_STATE_SPLIT 2048
'''
def function(text,name):
    # Selected original functions, including their parameter list and body.
    m=re.search(r'(?:static\s+)?(?:inline\s+)?(?:uint32_t|uint64_t|void)\s+'+name+r'\s*\(',text)
    assert m,name
    start=m.start();body=text.index('{',m.end());depth=0
    for i in range(body,len(text)):
        depth+= (text[i]=='{')-(text[i]=='}')
        if depth==0:return text[start:i+1]
    raise ValueError(name)
start=s.index('static struct {');end=s.index('static const uint32_t ps_kernel_gen9');globals=s[start:end]
start=s.index('static const uint32_t gen12_render_copy[][4]');end=s.index('\n};',start)+3;shader=s[start:end]
# Dependency order retains entire selected function bodies. Non-selected
# compression/PXP operations are forbidden by the supplied linear inputs.
funcs=[]
for name in ['emit_vertex_2s','emit_vertex','emit_vertex_normalized']:
    funcs.append(function((root/'rendercopy.h').read_text(),name))
for name in ['lnl_compression_format','dg2_compression_format','gen9_bind_buf','gen8_bind_surfaces','gen8_create_sampler','gen8_fill_ps','fast_clear_scale','gen7_fill_vertex_buffer_data','gen6_emit_vertex_elements','gen7_emit_vertex_buffer','gen6_create_cc_state','gen8_create_blend_state','gen6_create_cc_viewport','gen7_create_sf_clip_viewport','gen6_create_scissor_rect','gen8_emit_sip','gen7_emit_push_constants','gen9_emit_state_base_address','gen7_emit_urb','gen8_emit_cc','gen8_emit_multisample','gen8_emit_vs','gen8_emit_hs','gen8_emit_gs','gen9_emit_ds','gen8_emit_wm_hz_op','gen8_emit_null_state','gen7_emit_clip','gen8_emit_sf','gen8_emit_ps','gen9_emit_depth','gen7_emit_clear','gen6_emit_drawing_rectangle','gen8_emit_vf_topology','gen8_emit_primitive','gen12_emit_pxp_state']:
    funcs.append(function(s,name))
a=(root/'intel_aux_pgtable.c').read_text()
for name in ['gen12_create_aux_pgtable_state','gen12_emit_aux_pgtable_state']:funcs.append(function(a,name))
funcs.append(function(s,'_gen9_render_op'))
main=r'''
int main(void){struct intel_bb b={.handle=3,.devid=0x46d0,.batch_offset=0x30000,.fd=-1};
struct intel_buf src={.bpp=32,.depth=32,.width=64,.height=64,.handle=1,.mocs_index=3,.surface={{.stride=256}},.addr={.offset=0x11000}};
struct intel_buf dst=src;dst.handle=2;dst.addr.offset=0x21000;
_gen9_render_op(&b,&src,0,0,64,64,&dst,0,0,NULL,NULL,gen12_render_copy,sizeof(gen12_render_copy));
printf("%u\n",b.ptr);for(int i=0;i<1024;i++){uint32_t n;memcpy(&n,b.batch+i*4,4);printf("%08x\n",n);}return 0;}
'''
# Preserve source grants in temporary material too.
definitions='\n'.join(l for l in s.splitlines() if l.lstrip().startswith('#define'))+'\n'
code='/*\n'+(root/'COPYING').read_text()+'\n*/\n'+prefix+definitions+globals+shader+'\n'+'\n'.join(funcs)+main
out=work/'igt-page.c';out.write_text(code)
cmd=['gcc','-std=c11','-O2','-Werror','-D__EXPORTED_HEADERS__','-D__user=','-I'+str(work),'-I'+str(root),str(out),'-o',str(out.with_suffix(''))]
r=subprocess.run(cmd,capture_output=True,text=True)
assert r.returncode==0,r.stderr
r=subprocess.run([str(out.with_suffix(''))],capture_output=True,text=True)
assert r.returncode==0,r.stderr
sys.stdout.write(r.stdout)
