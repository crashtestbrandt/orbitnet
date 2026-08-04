{
  description = "Development environment for OrbitNet — rollback netcode for Godot 4, in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs, ... }:
    let
      supportedSystems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      godotVersion = "4.7-stable";
      godotTemplateVersion = "4.7.stable";
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };

          godotRuntimeLibs = with pkgs; [
            alsa-lib
            dbus
            fontconfig
            freetype
            glib
            libdecor
            libglvnd
            libpulseaudio
            libxkbcommon
            libx11
            libxcursor
            libxext
            libxfixes
            libxi
            libxinerama
            libxrandr
            libxrender
            speechd
            stdenv.cc.cc.lib
            systemdLibs
            vulkan-loader
            wayland
            zlib
          ];

          godotBin = pkgs.stdenv.mkDerivation {
            pname = "godot-bin";
            version = godotVersion;

            src = pkgs.fetchurl {
              url = "https://github.com/godotengine/godot-builds/releases/download/${godotVersion}/Godot_v${godotVersion}_linux.x86_64.zip";
              hash = "sha256-CxpsVMLGGcEuFp/pJB7dpLgQgLUZRRzsKYS/DSxstzw=";
            };

            nativeBuildInputs = with pkgs; [
              autoPatchelfHook
              unzip
            ];
            buildInputs = godotRuntimeLibs;
            dontUnpack = true;

            installPhase = ''
              runHook preInstall
              unzip -q "$src"
              install -Dm755 Godot_v${godotVersion}_linux.x86_64 "$out/bin/godot"
              runHook postInstall
            '';

            meta = {
              description = "Official Godot ${godotVersion} Linux editor binary";
              homepage = "https://godotengine.org";
              license = pkgs.lib.licenses.mit;
              mainProgram = "godot";
              platforms = [ "x86_64-linux" ];
            };
          };

          godotExportTemplates = pkgs.stdenvNoCC.mkDerivation {
            pname = "godot-export-templates";
            version = godotVersion;

            src = pkgs.fetchurl {
              url = "https://github.com/godotengine/godot-builds/releases/download/${godotVersion}/Godot_v${godotVersion}_export_templates.tpz";
              hash = "sha256-lxRFncBxkHwPPV8X1gj69p582iEzH8XTnEUD/6Tpnuw=";
            };

            nativeBuildInputs = [ pkgs.unzip ];
            dontUnpack = true;

            installPhase = ''
              runHook preInstall
              mkdir -p "$out/share/godot/export_templates/${godotTemplateVersion}"
              unzip -q "$src" "templates/version.txt" "templates/linux*"
              cp templates/* "$out/share/godot/export_templates/${godotTemplateVersion}/"
              runHook postInstall
            '';

            meta = {
              description = "Official Godot ${godotVersion} Linux export templates (client + dedicated server)";
              homepage = "https://godotengine.org";
              license = pkgs.lib.licenses.mit;
              platforms = [ "x86_64-linux" ];
            };
          };

          godot = pkgs.writeShellApplication {
            name = "godot";
            runtimeInputs = [ pkgs.coreutils ];
            text = ''
              data_root="''${ORBITNET_GODOT_DATA:-$PWD/.godot-nix/data}"
              cache_root="''${ORBITNET_GODOT_CACHE:-$PWD/.godot-nix/cache}"
              config_root="''${ORBITNET_GODOT_CONFIG:-$PWD/.godot-nix/config}"
              template_dir="$data_root/godot/export_templates"

              mkdir -p "$template_dir"
              mkdir -p "$cache_root" "$config_root"
              ln -sfn "${godotExportTemplates}/share/godot/export_templates/${godotTemplateVersion}" "$template_dir/${godotTemplateVersion}"

              export XDG_DATA_HOME="$data_root"
              export XDG_CACHE_HOME="$cache_root"
              export XDG_CONFIG_HOME="$config_root"
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath godotRuntimeLibs}:''${LD_LIBRARY_PATH:-}"
              exec "${godotBin}/bin/godot" "$@"
            '';
          };

        in
        {
          default = godot;
          inherit
            godot
            godotBin
            godotExportTemplates
            ;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          orbitnetPackages = self.packages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              curl
              just
              python3
              unzip
              xvfb-run   # a GL context for a non-headless run on a box with no display server
              zip
              orbitnetPackages.godot

              # Rust, for the native GDExtension in native/.
              # `rustup` rather than nixpkgs' rustc/cargo deliberately: the workspace pins an exact
              # toolchain in native/rust-toolchain.toml (gdext requires >= 1.94), and rustup honours
              # that pin automatically, whereas nixpkgs' Rust floats with the unstable channel and
              # would silently drift off it. The trade-off is that rustup fetches the toolchain on
              # first use instead of it being pure.
              rustup
              pkg-config
            ];

            shellHook = ''
              export GODOT="godot"
              export ORBITNET_ROOT="$PWD"
              export ORBITNET_GODOT_DATA="$PWD/.godot-nix/data"
              export ORBITNET_GODOT_CACHE="$PWD/.godot-nix/cache"
              export ORBITNET_GODOT_CONFIG="$PWD/.godot-nix/config"

              echo "OrbitNet dev shell: Godot ${godotVersion}, $(rustup --version 2>/dev/null | head -1)"
              echo "First run:  just sync-addons && just rts"
            '';
          };
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.godot}/bin/godot";
        };
      });

      formatter = forAllSystems (system: (import nixpkgs { inherit system; }).nixfmt);
    };
}
