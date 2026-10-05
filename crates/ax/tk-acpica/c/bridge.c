/* Original TheKernel FFI/variadic adapter, Apache-2.0. */
#include "acpi.h"
#include "accommon.h"
#include "acresrc.h"
extern void tk_acpi_log(const char *, ACPI_SIZE);
void AcpiOsVprintf(const char *format, va_list args) {
    char buffer[512];
    int length = vsnprintf(buffer, sizeof(buffer), format, args);
    if (length > 0) tk_acpi_log(buffer, (ACPI_SIZE)length < sizeof(buffer) ? (ACPI_SIZE)length : sizeof(buffer)-1);
}
void AcpiOsPrintf(const char *format, ...) {
    va_list args; va_start(args, format); AcpiOsVprintf(format, args); va_end(args);
}

/* An owned, bounded wire representation avoids exposing ACPICA object unions
 * and embedded pointers to Rust. Length/tag are little-endian x86_64 scalars. */
static ACPI_STATUS WireObject(ACPI_OBJECT *object, UINT8 *out, ACPI_SIZE capacity,
    ACPI_SIZE *used, unsigned int depth) {
    UINT32 tag = object->Type, count = 0, i;
    const void *payload = NULL;
    UINT64 integer;
    ACPI_BUFFER name = { ACPI_ALLOCATE_BUFFER, NULL };
    ACPI_STATUS status = AE_OK;
    if (depth > 32 || *used > capacity || capacity - *used < 8) return AE_LIMIT;
    switch (tag) {
    case ACPI_TYPE_ANY: break;
    case ACPI_TYPE_INTEGER: count = 8; integer = object->Integer.Value; payload = &integer; break;
    case ACPI_TYPE_STRING: count = object->String.Length; payload = object->String.Pointer; break;
    case ACPI_TYPE_BUFFER: count = object->Buffer.Length; payload = object->Buffer.Pointer; break;
    case ACPI_TYPE_PACKAGE: count = object->Package.Count; break;
    case ACPI_TYPE_LOCAL_REFERENCE:
        status = AcpiGetName(object->Reference.Handle, ACPI_FULL_PATHNAME, &name);
        if (ACPI_FAILURE(status)) return status;
        count = strlen(name.Pointer); payload = name.Pointer; break;
    default: return AE_TYPE;
    }
    memcpy(out + *used, &tag, 4); memcpy(out + *used + 4, &count, 4); *used += 8;
    if (tag == ACPI_TYPE_PACKAGE) {
        if (count > 65536) return AE_LIMIT;
        for (i = 0; i < count; i++) {
            status = WireObject(&object->Package.Elements[i], out, capacity, used, depth+1);
            if (ACPI_FAILURE(status)) break;
        }
    } else if (count > capacity - *used) { status = AE_LIMIT; }
    else if (count) { memcpy(out + *used, payload, count); *used += count; }
    if (name.Pointer) AcpiOsFree(name.Pointer);
    return status;
}
ACPI_STATUS tk_acpi_evaluate(const char *path, UINT64 *integers, UINT32 argc,
    UINT8 *out, ACPI_SIZE capacity, ACPI_SIZE *used) {
    ACPI_OBJECT args[4]; ACPI_OBJECT_LIST list = {argc, args};
    ACPI_BUFFER result = {ACPI_ALLOCATE_BUFFER, NULL};
    ACPI_STATUS status; UINT32 i;
    if (argc > 4) return AE_BAD_PARAMETER;
    for (i=0;i<argc;i++) { args[i].Type=ACPI_TYPE_INTEGER;args[i].Integer.Value=integers[i]; }
    *used=0;
    status=AcpiEvaluateObject(NULL,(char *)path,argc ? &list : NULL,&result);
    if (ACPI_SUCCESS(status) && result.Pointer)
        status=WireObject(result.Pointer,out,capacity,used,0);
    if (result.Pointer) AcpiOsFree(result.Pointer);
    return status;
}
typedef ACPI_STATUS (*TK_NODE_CALLBACK)(const char *, UINT32, void *);
struct TK_WALK { TK_NODE_CALLBACK callback; void *context; };
static ACPI_STATUS WalkNode(ACPI_HANDLE handle, UINT32 depth, void *context, void **ret) {
    struct TK_WALK *walk=context; ACPI_BUFFER path={ACPI_ALLOCATE_BUFFER,NULL};
    ACPI_OBJECT_TYPE type; ACPI_STATUS status=AcpiGetName(handle,ACPI_FULL_PATHNAME,&path);
    (void)depth;(void)ret;
    if (ACPI_SUCCESS(status)) status=AcpiGetType(handle,&type);
    if (ACPI_SUCCESS(status)) status=walk->callback(path.Pointer,type,walk->context);
    if (path.Pointer) AcpiOsFree(path.Pointer);
    return status;
}
ACPI_STATUS tk_acpi_walk(TK_NODE_CALLBACK callback, void *context) {
    struct TK_WALK walk={callback,context};
    return AcpiWalkNamespace(ACPI_TYPE_ANY,ACPI_ROOT_OBJECT,64,WalkNode,NULL,&walk,NULL);
}
extern void tk_acpi_notify(const char *, UINT32);
static void Notify(ACPI_HANDLE handle, UINT32 value, void *context) {
    ACPI_BUFFER path={ACPI_ALLOCATE_BUFFER,NULL}; (void)context;
    if (ACPI_SUCCESS(AcpiGetName(handle,ACPI_FULL_PATHNAME,&path))) {
        tk_acpi_notify(path.Pointer,value); AcpiOsFree(path.Pointer);
    }
}
ACPI_STATUS tk_acpi_install_notify(void) {
    return AcpiInstallNotifyHandler(ACPI_ROOT_OBJECT,ACPI_ALL_NOTIFY,Notify,NULL);
}
void tk_acpi_remove_notify(void) {
    AcpiRemoveNotifyHandler(ACPI_ROOT_OBJECT,ACPI_ALL_NOTIFY,Notify);
}
/* Validate decoded resource lists as well as returning the original AML buffer. */
ACPI_STATUS tk_acpi_resources(const char *path, UINT8 possible,
    UINT8 *out, ACPI_SIZE capacity, ACPI_SIZE *used) {
    ACPI_HANDLE handle;
    ACPI_BUFFER resources={ACPI_ALLOCATE_BUFFER,NULL}, aml={ACPI_ALLOCATE_BUFFER,NULL};
    ACPI_STATUS status=AcpiGetHandle(NULL,(char *)path,&handle);
    *used=0;
    if (ACPI_SUCCESS(status)) status=possible ? AcpiGetPossibleResources(handle,&resources) : AcpiGetCurrentResources(handle,&resources);
    if (ACPI_SUCCESS(status)) status=AcpiRsCreateAmlResources(&resources,&aml);
    if (ACPI_SUCCESS(status)) {
        if(aml.Length>capacity)status=AE_LIMIT;
        else{memcpy(out,aml.Pointer,aml.Length);*used=aml.Length;}
    }
    if(resources.Pointer)AcpiOsFree(resources.Pointer);
    if(aml.Pointer)AcpiOsFree(aml.Pointer);
    return status;
}
/* _OSC only offers support already implemented by the OS; no native PCIe
 * hotplug/AER/PME ownership is requested by this first integration. */
ACPI_STATUS tk_acpi_platform_osc(void) {
    UINT8 uuid[16]={0xA6,0x13,0x0C,0x08,0x70,0xEC,0x26,0x4D,0xBF,0xD7,0x20,0x13,0xE3,0xB8,0x4B,0x6E};
    UINT32 capabilities[2]={1,0};
    ACPI_OBJECT args[4]; ACPI_OBJECT_LIST list={4,args};
    ACPI_BUFFER result={ACPI_ALLOCATE_BUFFER,NULL}; ACPI_STATUS status;
    args[0].Type=ACPI_TYPE_BUFFER;args[0].Buffer.Length=16;args[0].Buffer.Pointer=uuid;
    args[1].Type=ACPI_TYPE_INTEGER;args[1].Integer.Value=1;
    args[2].Type=ACPI_TYPE_INTEGER;args[2].Integer.Value=2;
    args[3].Type=ACPI_TYPE_BUFFER;args[3].Buffer.Length=8;args[3].Buffer.Pointer=(UINT8 *)capabilities;
    status=AcpiEvaluateObject(NULL,"\\_SB._OSC",&list,&result);
    if (ACPI_SUCCESS(status)) {
        ACPI_OBJECT *o=result.Pointer;
        if (!o || o->Type != ACPI_TYPE_BUFFER || o->Buffer.Length != 8) status=AE_TYPE;
        else if (((UINT32 *)o->Buffer.Pointer)[0] & 0x1e) status=AE_SUPPORT;
    }
    if(result.Pointer)AcpiOsFree(result.Pointer);
    return status;
}
extern void tk_acpi_fixed_power(void);
static UINT32 FixedPower(void *context) { (void)context;tk_acpi_fixed_power();return ACPI_INTERRUPT_HANDLED; }
ACPI_STATUS tk_acpi_install_fixed_power(void) {
    return AcpiInstallFixedEventHandler(ACPI_EVENT_POWER_BUTTON,FixedPower,NULL);
}
ACPI_STATUS tk_acpi_table(UINT32 index, UINT8 *out, ACPI_SIZE capacity, ACPI_SIZE *used) {
    ACPI_TABLE_HEADER *table;ACPI_STATUS status=AcpiGetTableByIndex(index,&table);*used=0;
    if(ACPI_FAILURE(status))return status;
    if(!table || table->Length<8 || table->Length>capacity)return AE_LIMIT;
    memcpy(out,table,table->Length);*used=table->Length;return AE_OK;
}
extern ACPI_STATUS tk_acpi_ec_access(UINT32,UINT64,UINT32,UINT64 *,void *);
static ACPI_STATUS EcAccess(UINT32 f,ACPI_PHYSICAL_ADDRESS a,UINT32 w,UINT64 *v,void *h,void *r) {
    (void)r;return tk_acpi_ec_access(f,a,w,v,h);
}
static ACPI_STATUS EcSetup(ACPI_HANDLE region,UINT32 function,void *context,void **region_context) {
    (void)region;*region_context=function==ACPI_REGION_ACTIVATE?context:NULL;return AE_OK;
}
ACPI_STATUS tk_acpi_install_ec(const char *path,void *context) {
    ACPI_HANDLE handle;ACPI_STATUS status=AcpiGetHandle(NULL,(char *)path,&handle);
    if(ACPI_FAILURE(status))return status;
    return AcpiInstallAddressSpaceHandler(handle,ACPI_ADR_SPACE_EC,EcAccess,EcSetup,context);
}
ACPI_STATUS tk_acpi_resolve(const char *parent,const char *source,UINT8 *out,ACPI_SIZE capacity,ACPI_SIZE *used) {
    ACPI_HANDLE base,handle;ACPI_BUFFER path={ACPI_ALLOCATE_BUFFER,NULL};
    ACPI_STATUS status=AcpiGetHandle(NULL,(char *)parent,&base);*used=0;
    if(ACPI_SUCCESS(status))status=AcpiGetHandle(base,(char *)source,&handle);
    if(ACPI_SUCCESS(status))status=AcpiGetName(handle,ACPI_FULL_PATHNAME,&path);
    if(ACPI_SUCCESS(status)){ACPI_SIZE length=strlen(path.Pointer);if(length>capacity)status=AE_LIMIT;else{memcpy(out,path.Pointer,length);*used=length;}}
    if(path.Pointer)AcpiOsFree(path.Pointer);return status;
}
UINT32 tk_acpi_table_count(void) {
    UINT32 count=0;
    if(ACPI_SUCCESS(AcpiUtAcquireMutex(ACPI_MTX_TABLES))) {
        count=AcpiGbl_RootTableList.CurrentTableCount;
        AcpiUtReleaseMutex(ACPI_MTX_TABLES);
    }
    return count;
}
