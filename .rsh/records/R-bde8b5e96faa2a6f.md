+++
id = "R-bde8b5e96faa2a6f"
created_at = "2026-08-29T11:21:26.990Z"
body_sha256 = "f3e7aef047507d4dcbe2237a5fc57bc4b05dc7fc856cf2c0943c0a7d8fbe651d"
kind = "finding"
title = "x86_64 creat retains kernel O_LARGEFILE"
summary = "Linux forces O_LARGEFILE in native creat and F_GETFL returns its kernel-visible bit even though glibc defines it as zero."
scope = "Linux x86_64 creat ABI."
topics = [ "linux-abi", "syscalls", "x86_64" ]
uses = []
related = [ "R-3f8613d0c1a92b82" ]
+++
# Conclusion

For native x86_64, implement creat as openat(AT_FDCWD, path, O_CREAT|O_WRONLY|O_TRUNC|O_LARGEFILE, mode). The OFD status snapshot must retain kernel bit 00100000 so F_GETFL reports it. Do not infer kernel behavior from glibc's x86_64 O_LARGEFILE value of zero.

# Evidence

Linux v6.12.103 fs/open.c SYSCALL_DEFINE2(creat) adds O_LARGEFILE when force_o_largefile() is true. The portable differential checks raw syscall 85, mode/umask, truncation, EFAULT, descriptor flags, and raw F_GETFL. TheKernel q35/TCG prints THEKERNEL_CREAT_OK with this behavior.

# Boundary

This closes only creat behavior. Linux open and openat independently force O_LARGEFILE and remain separate matrix contracts.
