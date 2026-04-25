{
  description = "Deptool";

  inputs.nixpkgs.url = "nixpkgs/nixos-25.11";

  # Flake adapted from Minimizer <https://codeberg.org/ruuda/minimizer>,
  # licensed Apache 2.0.
  outputs = { self, nixpkgs }: 
    let
      name = "deptool";
      version = "0.1.0";
      pkgs = (import nixpkgs { system = "x86_64-linux" ; });
      pkgss = pkgs.pkgsStatic;
    in
      {
        devShells.x86_64-linux.default = pkgs.mkShell {
          inherit name;
          nativeBuildInputs = [ pkgs.mkdocs ];
        };

        packages.x86_64-linux.default = pkgss.rustPlatform.buildRustPackage rec {
          inherit name version;
          src = pkgss.lib.sourceFilesBySuffices ./. [
            ".rs"
            "Cargo.lock"
            "Cargo.toml"
          ];
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "git2-0.21.0" = "sha256-Wr2uhMZHRM2ZEnU4YDlcG2YrGyJEk/wERTYjy/1EaRc=";
            };
          };
          nativeBuildInputs = [ pkgss.pkg-config pkgs.git ];
          buildInputs = with pkgss; [
            libgit2
            libssh2
            openssl
            pcre
            zlib
          ];
          BUILD_COMMIT =
            if builtins.hasAttr "rev" self
            then self.rev
            else throw "Deptool must be built from a clean tree.";
          RUSTFLAGS = "-lm -lssl -lc";

          # The tests must be compiled with debug assertions enabled for the
          # test binary to skip installation, like the production binary does.
          # But Nix builds tests in release mode, and then they fail inside the
          # sandbox. Just skip the tests then.
          doCheck = false;
        };
      };
}
