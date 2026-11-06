{
  description = "garde: greetd login screen and Wayland lock screen with one look";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/1c89083e451e891152365b799595cf7790a43325";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        garde = pkgs.callPackage ./nix/package.nix { };
        default = garde;
      });

      overlays.default = final: _prev: { garde = final.callPackage ./nix/package.nix { }; };

      nixosModules.default = import ./nix/module.nix self;

      checks = forAll (pkgs: {
        # Unit tests run in the package's checkPhase.
        garde = self.packages.${pkgs.stdenv.hostPlatform.system}.garde;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.garde ];
          packages = with pkgs; [ clippy rustfmt rust-analyzer ];
          XKB_CONFIG_ROOT = "${pkgs.xkeyboard_config}/share/X11/xkb";
        };
      });
    };
}
