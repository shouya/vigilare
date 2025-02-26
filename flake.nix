{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, utils, naersk }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        deps = [ pkgs.xdotool ];
        naersk-lib = pkgs.callPackage naersk {};
      in
      {
        defaultPackage = with pkgs; naersk-lib.buildPackage {
          buildInputs = deps;
          src = ./.;
          preBuild = ''
            export NIX_LDFLAGS="$NIX_LDFLAGS -L${pkgs.lib.makeLibraryPath deps}"
          '';
        };
        devShell = with pkgs; mkShell {
          buildInputs = [
            cargo
            rustc
            rustfmt
            pre-commit
            rustPackages.clippy
            rust-analyzer
          ];

          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };
      }
    );
}
