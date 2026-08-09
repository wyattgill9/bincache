{ ... }:
{
  perSystem =
    {
      pkgs,
      rustToolchain,
      lib,
      ...
    }:
    {
      devShells.default = pkgs.mkShell {
        nativeBuildInputs = [
          rustToolchain

          pkgs.sccache
          pkgs.clang
          pkgs.lldb

          pkgs.cargo-nextest
          pkgs.cargo-llvm-cov

          # scripts/conformance.py and scripts/e2e-nix.py, run via `direnv exec . python3`.
          (pkgs.python3.withPackages (ps: [
            ps.cryptography
            ps.zstandard
          ]))
        ]
        # mold and wild are ELF-only, so Linux-only.
        ++ lib.optionals pkgs.stdenv.isLinux [
          pkgs.mold
          pkgs.wild
        ];

        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

        shellHook = ''
          export RUSTC_WRAPPER=sccache
        ''
        + lib.optionalString pkgs.stdenv.isLinux ''
          # Pick the linker with LINKER=wild (or LINKER=lld, LINKER=bfd, ...).
          export RUSTFLAGS="''${RUSTFLAGS:-} -C link-arg=-fuse-ld=''${LINKER:-mold}"
        '';
      };
    };
}
