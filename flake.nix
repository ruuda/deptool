{
  description = "Deptool";

  inputs.nixpkgs.url = "nixpkgs/nixos-25.11";

  # Flake adapted from Minimizer <https://codeberg.org/ruuda/minimizer>,
  # licensed Apache 2.0.
  outputs = { self, nixpkgs }: 
    let
      name = "deptool";
      version = "0.1.0";
      pkgs = (import nixpkgs { system = "x86_64-linux" ; }).pkgsStatic;
    in
      {
        packages.x86_64-linux.default = pkgs.rustPlatform.buildRustPackage rec {
          inherit name version;
          src = pkgs.lib.sourceFilesBySuffices ./. [
            ".rs"
            "Cargo.lock"
            "Cargo.toml"
          ];
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "git2-0.21.0" = "sha256-y+uOGVQEEotOKWXxx7NOIDo4HiGoqcNJXLEv5cow2eA=";
            };
          };
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = with pkgs; [
            libgit2
            libssh2
            openssl
            pcre
            zlib
          ];
          RUSTFLAGS = "-lm -lssl -lc";
        };
      };
}
