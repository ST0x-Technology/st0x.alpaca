{
  description = "st0x.alpaca: shared Alpaca client library for st0x services.";

  inputs = {
    rainix.url = "github:rainprotocol/rainix";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      flake-utils,
      rainix,
      crane,
      ...
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-darwin" ] (
      system:
      let
        pkgs = rainix.pkgs.${system};
        craneLib = (crane.mkLib pkgs).overrideToolchain rainix.rust-toolchain.${system};

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
          pname = "st0x-alpaca-gateway";
          version = "0.1.0";
          cargoExtraArgs = "--locked -p st0x-alpaca-gateway";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # The gateway binary. Tests run in CI through `cargo test`, not here.
        # The commit goes into the version every audit record carries; only
        # this derivation sees it, so the dependency build stays cached
        # across commits.
        st0x-alpaca-gateway = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false;
            ST0X_ALPACA_GATEWAY_REV = self.rev or self.dirtyRev or "unknown";
          }
        );

        # OCI image for Cloud Run. No base image and a pinned `created`, so a
        # commit rebuilds to the same digest (the st0x.bebop and
        # st0x.liquidity convention). The service reads its config path from
        # ST0X_ALPACA_GATEWAY_CONFIG, which the deployment mounts from Secret
        # Manager.
        # Streamed like st0x.liquidity's and st0x.pricing's images:
        # `nix build .#gateway-oci && ./result | docker load`. The binary is
        # also at /bin so the release workflow can run
        # `/bin/st0x-alpaca-gateway --validate-config /candidate.toml`.
        gateway-oci = pkgs.dockerTools.streamLayeredImage {
          name = "t0-alpaca";
          tag = "latest";
          created = "1970-01-01T00:00:01Z";
          contents = [
            pkgs.cacert
            st0x-alpaca-gateway
          ];
          config = {
            Entrypoint = [ "/bin/st0x-alpaca-gateway" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "ST0X_ALPACA_GATEWAY_CONFIG=/run/t0-alpaca/gateway.toml"
            ];
            ExposedPorts = {
              "8080/tcp" = { };
            };
            User = "65534:65534";
          };
        };
        isLinux = pkgs.stdenv.hostPlatform.isLinux;
      in
      {
        # The gateway exists only to ship in the Linux OCI image, and a native
        # build on darwin would put a Mach O binary into it, so both outputs
        # exist only on Linux systems. Darwin keeps the dev shell.
        packages =
          rainix.packages.${system}
          // pkgs.lib.optionalAttrs isLinux {
            inherit st0x-alpaca-gateway gateway-oci;
          };

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
