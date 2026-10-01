{
  description = "ene Rust development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      toolchain = (fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain;
      rustComponents = with pkgs; [ rustc cargo rustfmt clippy ];
    in
    {
      devShells.${system}.default =
        assert pkgs.lib.assertMsg
          (builtins.all (component: component.version == toolchain.channel) rustComponents)
          "Nix Rust, Cargo, rustfmt and Clippy must match rust-toolchain.toml";
        pkgs.mkShell {
          packages = rustComponents ++ (with pkgs; [
            rust-analyzer
            pkg-config
            git
            just
          ]);

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
    };
}
