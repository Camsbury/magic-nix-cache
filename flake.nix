{
  description = "GitHub Actions-powered Nix binary cache";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";

    crane.url = "https://flakehub.com/f/ipetkov/crane/*";

    nix.url = "https://flakehub.com/f/NixOS/nix/=2.27.*";
  };

  outputs = inputs:
    let
      supportedSystems = [
        "aarch64-linux"
        "x86_64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      forEachSupportedSystem = f: inputs.nixpkgs.lib.genAttrs supportedSystems (system: f rec {
        pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [
            inputs.self.overlays.default
          ];
        };
        inherit system;
      });
    in
    {

      overlays.default = final: prev:
      let
          craneLib = inputs.crane.mkLib final;
          crateName = craneLib.crateNameFromCargoToml {
            cargoToml = ./magic-nix-cache/Cargo.toml;
          };

          commonArgs = {
            inherit (crateName) pname version;
            src = inputs.self;

            nativeBuildInputs = with final; [
              pkg-config
              protobuf
            ];

            buildInputs = [
              inputs.nix.packages.${final.stdenv.system}.default
              final.boost
            ];
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in
      {
        magic-nix-cache = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });
      }
      // prev.lib.optionalAttrs prev.stdenv.hostPlatform.isLinux (
        let
          # Use a musl *cross* package set, so build scripts/proc-macros are
          # still built for the native (glibc) host.
          pkgsMusl =
            if prev.stdenv.hostPlatform.system == "x86_64-linux" then
              prev.pkgsCross.musl64
            else if prev.stdenv.hostPlatform.system == "aarch64-linux" then
              prev.pkgsCross.aarch64-multiplatform-musl
            else
              throw "magic-nix-cache-static is only supported on Linux";

          craneLibStatic = inputs.crane.mkLib pkgsMusl;

          atticConfigHeaders = pkgsMusl.buildPackages.runCommand "attic-config-headers" { } ''
            mkdir -p "$out"

            # Headers `attic` force-includes, but which aren't shipped by `nixStatic`.
            cat > "$out/config-store.hh" <<'EOF'
            // Placeholder config-store header
            #pragma once
            EOF

            cat > "$out/config-main.hh" <<'EOF'
            // Placeholder config-main header
            #pragma once
            EOF

            # Compatibility shims: `attic` expects older Nix header layout where
            # many headers were in the top-level include directory.
            cat > "$out/store-api.hh" <<'EOF'
            #pragma once

            #include <map>

            #include <nix/store/store-api.hh>
            #include <nix/store/store-open.hh>
            #include <nix/store/globals.hh>

            // Compatibility shim for older `attic` expecting
            // `openStore(std::string, std::map<std::string, std::string>)`.
            namespace nix {
            inline ref<Store> openStore(
                const std::string & uri,
                const std::map<std::string, std::string> & extraParams)
            {
                StoreReference::Params params;
                for (auto const & kv : extraParams) params.emplace(kv.first, kv.second);
                return openStore(uri, params);
            }
            } // namespace nix
            EOF

            cat > "$out/local-store.hh" <<'EOF'
            #pragma once
            #include <nix/store/local-store.hh>
            EOF

            cat > "$out/remote-store.hh" <<'EOF'
            #pragma once
            #include <nix/store/remote-store.hh>
            EOF

            cat > "$out/uds-remote-store.hh" <<'EOF'
            #pragma once
            #include <nix/store/uds-remote-store.hh>
            EOF

            cat > "$out/path.hh" <<'EOF'
            #pragma once
            #include <nix/store/path.hh>
            EOF

            cat > "$out/hash.hh" <<'EOF'
            #pragma once
            #include <nix/util/hash.hh>
            EOF

            cat > "$out/serialise.hh" <<'EOF'
            #pragma once
            #include <nix/util/serialise.hh>
            EOF

            cat > "$out/shared.hh" <<'EOF'
            #pragma once
            #include <nix/main/shared.hh>
            EOF
          '';

          commonArgsStatic =
            let
              # When linking against Nix's static libraries (`nixStatic`) we
              # need to explicitly list some transitive native dependencies.
              #
              # With dynamic linking, these would be pulled in automatically via
              # DT_NEEDED entries; with static archives, the linker only pulls
              # in objects for libraries that appear *after* the objects that
              # need them.
              #
              # We add these as extra `rustc` linker args so they come at the
              # end of the link line (and therefore satisfy ordering).
              extraStaticRustLinkArgs = [
                # Nix uses several Boost components.
                "-C link-arg=-lboost_context"
                "-C link-arg=-lboost_iostreams"
                "-C link-arg=-lboost_url"
                "-C link-arg=-lboost_system"

                # libarchive optional deps
                "-C link-arg=-llzma"
                "-C link-arg=-lbz2"
                "-C link-arg=-lacl"

                # Ensure common deps appear after users (static link ordering).
                "-C link-arg=-lbrotlicommon"
                "-C link-arg=-lunistring"
                "-C link-arg=-lssl"
                "-C link-arg=-lcrypto"

                # Some static libs reference symbols in libc/pthread, but `rustc`
                # puts `-lc`/`-lpthread` earlier on the link line. Re-add them at
                # the end so the static link order resolves correctly.
                "-C link-arg=-lpthread"
                "-C link-arg=-lc"
              ];

              rustflagsStatic = builtins.concatStringsSep " " (
                [ "-C target-feature=+crt-static" ] ++ extraStaticRustLinkArgs
              );
            in
            {
              inherit (crateName) pname version;
              src = inputs.self;

              nativeBuildInputs = with pkgsMusl.buildPackages; [
                pkg-config
                protobuf
              ];

              buildInputs = [
                pkgsMusl.nixStatic
                pkgsMusl.boost

                # Provide static archives for transitive deps we link explicitly.
                pkgsMusl.xz
                pkgsMusl.bzip2
                pkgsMusl.acl
              ];

              # Force the *target* (musl) build to be static, without forcing
              # the host build (build scripts, proc-macros) to be static.
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = rustflagsStatic;
              CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = rustflagsStatic;
              PKG_CONFIG_ALL_STATIC = "1";

              # `attic`'s C++ bindings (used as a dependency) pass
              # `-include config-store.hh` / `-include config-main.hh` but these
              # headers are not shipped by `nixStatic`.
              #
              # Provide minimal placeholder headers and add them to the include
              # path for the *target* (musl) C++ compiler.
              # Nix 2.31 headers require C++23 (e.g. `std::views::zip`).
              CXXFLAGS_x86_64_unknown_linux_musl = "-std=c++23 -I${atticConfigHeaders}";
              CXXFLAGS_aarch64_unknown_linux_musl = "-std=c++23 -I${atticConfigHeaders}";
            };

          cargoArtifactsStatic = craneLibStatic.buildDepsOnly commonArgsStatic;
        in
        {
          magic-nix-cache-static = craneLibStatic.buildPackage (commonArgsStatic // {
            cargoArtifacts = cargoArtifactsStatic;
          });
        }
      );

      packages = forEachSupportedSystem ({ pkgs, ... }:
        let
          lib = pkgs.lib;
        in
        (rec {
          magic-nix-cache = pkgs.magic-nix-cache;
          default = magic-nix-cache;
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          magic-nix-cache-static = pkgs.magic-nix-cache-static;
        }
        // {
          veryLongChain =
            let
              ctx = ./README.md;

              # Function to write the current date to a file
              startFile =
                pkgs.stdenv.mkDerivation {
                  name = "start-file";
                  buildCommand = ''
                    cat ${ctx} > $out
                  '';
                };

              # Recursive function to create a chain of derivations
              createChain = n: startFile:
                pkgs.stdenv.mkDerivation {
                  name = "chain-${toString n}";
                  src =
                    if n == 0 then
                      startFile
                    else createChain (n - 1) startFile;
                  buildCommand = ''
                    echo $src  > $out
                  '';
                };

            in
            # Starting point of the chain
            createChain 200 startFile;
        }));

      devShells = forEachSupportedSystem ({ system, pkgs }: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer

            inputs.nix.packages.${stdenv.system}.default # for linking attic
            boost # for linking attic
            protobuf # for protoc/prost
            bashInteractive
            pkg-config

            cargo-bloat
            cargo-edit
            cargo-udeps
            cargo-watch
            bacon

            age
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustcSrc}/library";
        };
      });
    };
}
