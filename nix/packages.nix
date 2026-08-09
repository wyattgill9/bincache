{ ... }:
{
  perSystem =
    {
      craneLib,
      ...
    }:
    let
      src = craneLib.cleanCargoSource ../.;

      # Nothing here links against a system library: zstd and mimalloc build their
      # own C, everything else is pure Rust. Add buildInputs when that stops being true.
      commonArgs = {
        inherit src;
        strictDeps = true;
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;

      bincache = craneLib.buildPackage (
        commonArgs
        // {
          inherit cargoArtifacts;
          pname = "bincache";
          cargoExtraArgs = "-p bincache";
        }
      );
    in
    {
      packages = {
        inherit bincache;
        default = bincache;
      };

      # `nix flake check` runs these plus treefmt (added by the treefmt-nix module).
      checks = {
        clippy = craneLib.cargoClippy (
          commonArgs
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          }
        );

        test = craneLib.cargoNextest (
          commonArgs
          // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          }
        );
      };
    };
}
