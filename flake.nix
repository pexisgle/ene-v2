{
  description = "ene Rust development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      desktopLibraries = with pkgs; [
        fontconfig
        libxkbcommon
        wayland
        libglvnd
        vulkan-loader
      ];
    in
    {
      devShells.${system}.default =
        pkgs.mkShell {
          packages = [ rustToolchain ] ++ (with pkgs; [
            clang
            lld
            pkg-config
            python3
            git
            just
          ]);

          buildInputs = desktopLibraries;

          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

          # Winit and WGPU load some libraries dynamically; use the host GPU driver.
          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath desktopLibraries}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };
    };
}
