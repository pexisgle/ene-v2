{
  description = "ene Rust development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      toolchain = (fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain;
      rustComponents = with pkgs; [ rustc cargo rustfmt clippy ];
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
        assert pkgs.lib.assertMsg
          (builtins.all (component: component.version == toolchain.channel) rustComponents)
          "Nix Rust, Cargo, rustfmt and Clippy must match rust-toolchain.toml";
        pkgs.mkShell {
          packages = rustComponents ++ (with pkgs; [
            rust-analyzer
            pkg-config
            python3
            git
            just
            bubblewrap
          ]);

          buildInputs = desktopLibraries;

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          # Winit and WGPU load some libraries dynamically; use the host GPU driver.
          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath desktopLibraries}:/run/opengl-driver/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };
    };
}
