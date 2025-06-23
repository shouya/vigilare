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
        deps = with pkgs; [ xdotool alsa-lib ];
        naersk-lib = pkgs.callPackage naersk {};
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
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
            pkg-config
          ] ++ deps;

          RUST_SRC_PATH = rustPlatform.rustLibSrc;
        };
      }
    );
}
