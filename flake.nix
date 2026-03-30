{
  description = "StarryOS development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";

    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        mkCompatCrossTools =
          {
            name,
            crossPkgs,
            expectedPrefix,
            actualPrefix ? crossPkgs.stdenv.cc.targetPrefix,
          }:
          pkgs.runCommand name { } ''
            mkdir -p "$out/bin"

            for tool in \
              addr2line ar as c++ c++filt cc cpp g++ gcc gprof ld ld.bfd nm \
              objcopy objdump ranlib readelf size strings strip
            do
              src="${crossPkgs.stdenv.cc}/bin/${actualPrefix}$tool"
              dst="$out/bin/${expectedPrefix}$tool"

              if [ -e "$src" ]; then
                ln -s "$src" "$dst"
              fi
            done
          '';

        crossCompatTools = pkgs.symlinkJoin {
          name = "starry-cross-compat-tools";
          paths = [
            (mkCompatCrossTools {
              name = "riscv64-musl-compat-tools";
              crossPkgs = pkgs.pkgsCross.riscv64-musl;
              expectedPrefix = "riscv64-linux-musl-";
            })
            (mkCompatCrossTools {
              name = "aarch64-musl-compat-tools";
              crossPkgs = pkgs.pkgsCross.aarch64-multiplatform-musl;
              expectedPrefix = "aarch64-linux-musl-";
            })
            (mkCompatCrossTools {
              name = "x86_64-musl-compat-tools";
              crossPkgs = pkgs.pkgsCross.musl64;
              expectedPrefix = "x86_64-linux-musl-";
            })
            (mkCompatCrossTools {
              name = "loongarch64-musl-compat-tools";
              crossPkgs = pkgs.pkgsCross.loongarch64-linux;
              actualPrefix = "loongarch64-unknown-linux-gnu-";
              expectedPrefix = "loongarch64-linux-musl-";
            })
          ];
        };
      in
      {
        packages.cross-tool-wrappers = crossCompatTools;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            cargo-binutils
            crossCompatTools
            cmake
            qemu
            ubootTools
            dosfstools
            gdb
            gnumake
            llvmPackages.libclang
            python3
            curl
            xz
            git
            pkg-config
          ];

          shellHook = ''
            export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
            export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
            export CARGO_INSTALL_ROOT="$PWD/.cargo/nix-tools"
            export PATH="$CARGO_INSTALL_ROOT/bin:$PATH"

            riscv64_gcc_include="$(riscv64-linux-musl-gcc -print-file-name=include)"
            aarch64_gcc_include="$(aarch64-linux-musl-gcc -print-file-name=include)"
            loongarch64_gcc_include="$(loongarch64-linux-musl-gcc -print-file-name=include)"
            x86_64_gcc_include="$(x86_64-linux-musl-gcc -print-file-name=include)"

            export CC_riscv64gc_unknown_none_elf=riscv64-linux-musl-gcc
            export AR_riscv64gc_unknown_none_elf=riscv64-linux-musl-ar
            export RANLIB_riscv64gc_unknown_none_elf=riscv64-linux-musl-ranlib
            export BINDGEN_EXTRA_CLANG_ARGS_riscv64gc_unknown_none_elf="--target=riscv64-unknown-linux-musl -isystem ${pkgs.pkgsCross.riscv64-musl.stdenv.cc.libc.dev}/include -isystem $riscv64_gcc_include"

            export CC_aarch64_unknown_none_softfloat=aarch64-linux-musl-gcc
            export AR_aarch64_unknown_none_softfloat=aarch64-linux-musl-ar
            export RANLIB_aarch64_unknown_none_softfloat=aarch64-linux-musl-ranlib
            export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_none_softfloat="--target=aarch64-unknown-linux-musl -isystem ${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc.libc.dev}/include -isystem $aarch64_gcc_include"

            export CC_loongarch64_unknown_none_softfloat=loongarch64-linux-musl-gcc
            export AR_loongarch64_unknown_none_softfloat=loongarch64-linux-musl-ar
            export RANLIB_loongarch64_unknown_none_softfloat=loongarch64-linux-musl-ranlib
            export BINDGEN_EXTRA_CLANG_ARGS_loongarch64_unknown_none_softfloat="--target=loongarch64-unknown-linux-gnu -isystem ${pkgs.pkgsCross.loongarch64-linux.stdenv.cc.libc.dev}/include -isystem $loongarch64_gcc_include"

            export CC_x86_64_unknown_none=x86_64-linux-musl-gcc
            export AR_x86_64_unknown_none=x86_64-linux-musl-ar
            export RANLIB_x86_64_unknown_none=x86_64-linux-musl-ranlib
            export BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_none="--target=x86_64-unknown-linux-musl -isystem ${pkgs.pkgsCross.musl64.stdenv.cc.libc.dev}/include -isystem $x86_64_gcc_include"

            if ! command -v cargo-axplat >/dev/null 2>&1 || ! command -v axconfig-gen >/dev/null 2>&1; then
              echo "StarryOS devShell: cargo-axplat and axconfig-gen are not packaged in nixpkgs yet."
              echo "The first make invocation will install missing cargo tools into $CARGO_INSTALL_ROOT."
            fi
          '';
        };
      }
    );
}
