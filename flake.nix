{
  description = "Dev shell for the app: Rust from nixpkgs, Apple SDK from the system Xcode";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      # macOS only until 1.0. Linux needs extra system libraries; add them then.
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAll (pkgs: {
        # NoCC on purpose: linking uses the system Xcode clang and SDK, which is
        # what GPUI's macOS backend expects. A nix cc wrapper brings its own SDK.
        default = pkgs.mkShellNoCC {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            cargo-deny
            # The term crate builds libghostty-vt from Ghostty's source with Zig.
            zig_0_17
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          shellHook = ''
            export DEVELOPER_DIR="''${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
          '';
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
