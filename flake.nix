{
  description = "Linux control daemon and CLI for the Creative Sound Blaster X1";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
    in {
      packages = forAll (pkgs: rec {
        x1ctl = pkgs.rustPlatform.buildRustPackage {
          pname = "x1ctl";
          version = "0.3.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          buildFeatures = [ "gui" ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postInstall = ''
            install -Dm644 packaging/70-sound-blaster-x1.rules $out/lib/udev/rules.d/70-sound-blaster-x1.rules
            install -Dm644 packaging/x1d.service $out/lib/systemd/user/x1d.service
            substituteInPlace $out/lib/systemd/user/x1d.service --replace-fail /usr/bin/x1d $out/bin/x1d
            install -Dm644 packaging/io.github.x1ctl.service $out/share/dbus-1/services/io.github.x1ctl.service
            substituteInPlace $out/share/dbus-1/services/io.github.x1ctl.service --replace-fail /usr/bin/x1d $out/bin/x1d
            install -Dm644 packaging/x1-tray.service $out/lib/systemd/user/x1-tray.service
            substituteInPlace $out/lib/systemd/user/x1-tray.service --replace-fail /usr/bin/x1-gui $out/bin/x1-gui
            install -Dm644 packaging/io.github.x1ctl.X1Gui.desktop $out/share/applications/io.github.x1ctl.X1Gui.desktop
            install -Dm644 packaging/io.github.x1ctl.X1Gui.svg $out/share/icons/hicolor/scalable/apps/io.github.x1ctl.X1Gui.svg
            install -Dm644 packaging/icon-256.png $out/share/icons/hicolor/256x256/apps/io.github.x1ctl.X1Gui.png
          '';
          # egui loads GL/Vulkan, Wayland and X11 at runtime via dlopen
          postFixup = ''
            wrapProgram $out/bin/x1-gui --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (with pkgs; [
              libGL vulkan-loader wayland libxkbcommon
              # X11 libs moved from xorg.* to top level in newer nixpkgs
              (pkgs.libx11 or pkgs.xorg.libX11) (pkgs.libxcursor or pkgs.xorg.libXcursor)
              (pkgs.libxrandr or pkgs.xorg.libXrandr) (pkgs.libxi or pkgs.xorg.libXi)
            ])}
          '';
          meta = {
            description = "Control Super X-Fi, graphic EQ, level and mute of the Creative Sound Blaster X1";
            license = pkgs.lib.licenses.mit;
            mainProgram = "x1ctl";
            platforms = pkgs.lib.platforms.linux;
          };
        };
        default = x1ctl;
      });

      nixosModules.default = { config, lib, pkgs, ... }:
        let cfg = config.services.x1ctl;
        in {
          options.services.x1ctl = {
            enable = lib.mkEnableOption "Sound Blaster X1 control (udev rule, x1d user service, x1ctl, x1-gui)";
            tray = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Start the X1 tray icon with the graphical session.";
            };
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            };
          };
          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
            services.udev.packages = [ cfg.package ];
            services.dbus.packages = [ cfg.package ];
            systemd.packages = [ cfg.package ];
            # start with the graphical/user session as well as on hotplug (udev SYSTEMD_USER_WANTS)
            systemd.user.services.x1d.wantedBy = [ "default.target" ];
            systemd.user.services.x1-tray.wantedBy = lib.mkIf cfg.tray [ "graphical-session.target" ];
          };
        };
    };
}
