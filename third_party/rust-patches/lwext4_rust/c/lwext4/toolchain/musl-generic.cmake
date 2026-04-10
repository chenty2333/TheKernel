if(NOT DEFINED ENV{ARCH})
    set(ARCH "x86_64")
else()
    set(ARCH $ENV{ARCH})
endif()

# Name of the target
set(CMAKE_SYSTEM_NAME "Linux")
set(CMAKE_SYSTEM_PROCESSOR ${ARCH})

# Toolchain settings
set(TOOLCHAIN_PREFIX ${ARCH}-linux-musl)

if(DEFINED ENV{CC} AND NOT "$ENV{CC}" STREQUAL "")
    set(CMAKE_C_COMPILER "$ENV{CC}")
else()
    set(CMAKE_C_COMPILER ${TOOLCHAIN_PREFIX}-cc)
endif()

if(DEFINED ENV{CXX} AND NOT "$ENV{CXX}" STREQUAL "")
    set(CMAKE_CXX_COMPILER "$ENV{CXX}")
else()
    set(CMAKE_CXX_COMPILER ${TOOLCHAIN_PREFIX}-c++)
endif()

if(DEFINED ENV{AS} AND NOT "$ENV{AS}" STREQUAL "")
    set(AS "$ENV{AS}")
else()
    set(AS ${TOOLCHAIN_PREFIX}-as)
endif()

if(DEFINED ENV{AR} AND NOT "$ENV{AR}" STREQUAL "")
    set(AR "$ENV{AR}")
else()
    set(AR ${TOOLCHAIN_PREFIX}-ar)
endif()

if(DEFINED ENV{OBJCOPY} AND NOT "$ENV{OBJCOPY}" STREQUAL "")
    set(OBJCOPY "$ENV{OBJCOPY}")
else()
    set(OBJCOPY ${TOOLCHAIN_PREFIX}-objcopy)
endif()

if(DEFINED ENV{OBJDUMP} AND NOT "$ENV{OBJDUMP}" STREQUAL "")
    set(OBJDUMP "$ENV{OBJDUMP}")
else()
    set(OBJDUMP ${TOOLCHAIN_PREFIX}-objdump)
endif()

if(DEFINED ENV{SIZE} AND NOT "$ENV{SIZE}" STREQUAL "")
    set(SIZE "$ENV{SIZE}")
else()
    set(SIZE ${TOOLCHAIN_PREFIX}-size)
endif()

set(LD_FLAGS "-nolibc -nostdlib -static --gc-sections -nostartfiles")

set(CMAKE_C_FLAGS   "-std=gnu99 -fdata-sections -ffunction-sections" CACHE INTERNAL "c compiler flags")
set(CMAKE_CXX_FLAGS "-fdata-sections -ffunction-sections" CACHE INTERNAL "cxx compiler flags")
set(CMAKE_ASM_FLAGS "" CACHE INTERNAL "asm compiler flags")

if(ARCH STREQUAL "x86_64")
    set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -mno-sse")
    set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -mno-sse")
elseif (ARCH STREQUAL "aarch64")
    set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -mgeneral-regs-only")
    set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -mgeneral-regs-only")
elseif (ARCH STREQUAL "riscv64")
    set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -march=rv64gc -mabi=lp64d -mcmodel=medany")
    set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -march=rv64gc -mabi=lp64d -mcmodel=medany")
elseif (ARCH STREQUAL "loongarch64")
    # Some environments expose LoongArch through a GNU hard-float cross
    # toolchain wrapper under the musl-compatible name. For lwext4's
    # freestanding C code we avoid forcing the soft-float ABI here so the
    # headers and sysroot stay consistent with the available toolchain.
endif()

set(CMAKE_C_FLAGS "-fPIC -fno-builtin -ffreestanding -fno-omit-frame-pointer ${CMAKE_C_FLAGS}")
set(CMAKE_CXX_FLAGS "-fPIC -nostdinc -fno-builtin -ffreestanding -fno-omit-frame-pointer ${CMAKE_CXX_FLAGS}")

if (APPLE)
    set(CMAKE_EXE_LINKER_FLAGS "-dead_strip" CACHE INTERNAL "exe link flags")
else (APPLE)
    set(CMAKE_EXE_LINKER_FLAGS "-Wl,--gc-sections" CACHE INTERNAL "exe link flags")
endif (APPLE)

SET(CMAKE_C_FLAGS_DEBUG "-O0 -g -ggdb3" CACHE INTERNAL "c debug compiler flags")
SET(CMAKE_CXX_FLAGS_DEBUG "-O0 -g -ggdb3" CACHE INTERNAL "cxx debug compiler flags")
SET(CMAKE_ASM_FLAGS_DEBUG "-g -ggdb3" CACHE INTERNAL "asm debug compiler flags")

SET(CMAKE_C_FLAGS_RELEASE "-O2 -g -ggdb3" CACHE INTERNAL "c release compiler flags")
SET(CMAKE_CXX_FLAGS_RELEASE "-O2 -g -ggdb3" CACHE INTERNAL "cxx release compiler flags")
SET(CMAKE_ASM_FLAGS_RELEASE "" CACHE INTERNAL "asm release compiler flags")
