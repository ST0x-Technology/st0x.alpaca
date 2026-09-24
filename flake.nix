{
  description = "st0x.alpaca: shared Alpaca client library for st0x services.";

  inputs = {
    rainix.url = "github:rainprotocol/rainix";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { flake-utils, rainix, ... }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-darwin" ] (
      system:
      let
        pkgs = rainix.pkgs.${system};
      in
      {
        packages = rainix.packages.${system};

        devShells.default = pkgs.mkShell {
          packages = [
            rainix.rust-toolchain.${system}
            pkgs.git
            pkgs.pkg-config
          ];
        };
      }
    );
}
