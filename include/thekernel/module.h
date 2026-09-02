/* Native x86-64 TheKernel module ABI, version 1. */
#ifndef THEKERNEL_MODULE_H
#define THEKERNEL_MODULE_H
#include <stdint.h>
#define THEKERNEL_MODULE_ABI_VERSION 1u
#define THEKERNEL_PARAM_SECTION ".thekernel.param.v1"
#define THEKERNEL_PARAM_ABI_V1 1u
#define THEKERNEL_PARAM_F_ARRAY 1u
enum thekernel_param_kind_v1 { THEKERNEL_PARAM_BOOL, THEKERNEL_PARAM_INT, THEKERNEL_PARAM_UINT, THEKERNEL_PARAM_LONG, THEKERNEL_PARAM_ULONG, THEKERNEL_PARAM_STRING, THEKERNEL_PARAM_CHARP };
struct thekernel_param_v1 { const char *name; void *arg; uint32_t *countp; uint16_t kind; uint16_t flags; uint32_t capacity; uint32_t reserved; };
struct thekernel_param_table_v1 { uint32_t abi_version, record_size, record_count, reserved; struct thekernel_param_v1 records[]; };

/* ET_REL entry points consumed by the kernel loader. */
int thekernel_module_init(void);
void thekernel_module_exit(void);

/* Stable, relocatable kernel exports for native v1 modules. */
uint32_t thekernel_module_abi_version(void);
void thekernel_module_yield(void);
uint32_t thekernel_module_current_pid(void);
uint32_t thekernel_module_current_cpu(void);
uint64_t thekernel_module_monotonic_time_ns(void);
#endif
