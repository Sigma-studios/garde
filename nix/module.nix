self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.garde;
  toml = pkgs.formats.toml { };

  # "us(intl)" -> { layout = "us"; variant = "intl"; }
  splitLayout = l:
    let m = builtins.match "([^(]+)\\(([^)]*)\\)" l;
    in if m == null then { layout = l; variant = ""; } else { layout = builtins.elemAt m 0; variant = builtins.elemAt m 1; };
  layouts = map splitLayout cfg.settings.layouts;
  xkbLayout = lib.concatMapStringsSep "," (l: l.layout) layouts;
  xkbVariant = lib.concatMapStringsSep "," (l: l.variant) layouts;

  configFile = toml.generate "garde.toml" (lib.filterAttrsRecursive (_: v: v != null) cfg.settings);

  cursor = cfg.login.cursor;
  greeter = pkgs.writeShellScript "garde-greeter" ''
    export XKB_DEFAULT_LAYOUT=${lib.escapeShellArg xkbLayout}
    ${lib.optionalString (lib.any (l: l.variant != "") layouts) "export XKB_DEFAULT_VARIANT=${lib.escapeShellArg xkbVariant}"}
    export XCURSOR_THEME=${lib.escapeShellArg cursor.name}
    export XCURSOR_SIZE=${toString cursor.size}
    ${lib.optionalString (cursor.package != null) "export XCURSOR_PATH=${cursor.package}/share/icons"}
    exec ${lib.getExe cfg.login.cagePackage} ${lib.escapeShellArgs cfg.login.cageArgs} -- ${lib.getExe cfg.package} login
  '';

  cosmicEnabled = config.services.desktopManager.cosmic.enable or false;
  cosmicGreeterPkg = config.services.displayManager.cosmic-greeter.package or pkgs.cosmic-greeter;
in
{
  options.services.garde = {
    enable = lib.mkEnableOption "garde, the login and lock screen";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "garde.packages.\${system}.default";
      description = "The garde package.";
    };

    settings = lib.mkOption {
      description = "Rendered to /etc/garde/config.toml.";
      default = { };
      type = lib.types.submodule {
        freeformType = toml.type;
        options = {
          user = lib.mkOption {
            type = lib.types.str;
            description = "The only user: shown on the card, never typed.";
          };
          wallpaper = lib.mkOption {
            type = lib.types.nullOr (lib.types.either lib.types.path lib.types.str);
            default = null;
            description = "Wallpaper image (same file as the desktop's). Must be readable by the greeter user.";
          };
          layouts = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ "us" ];
            example = [ "fr" "us" ];
            description = "xkb layouts (`code` or `code(variant)`), first is the default.";
          };
          numlock = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Turn num lock on at start.";
          };
          dim = lib.mkOption {
            type = lib.types.numbers.between 0 1;
            default = 0.35;
            description = "Darkening behind the card, 0 = none.";
          };
          blur = lib.mkOption {
            type = lib.types.numbers.nonnegative;
            default = 0;
            description = "Blur radius behind the card (logical px), 0 = off.";
          };
          font = lib.mkOption {
            type = lib.types.str;
            default = "Noto Sans";
            description = "Font family.";
          };
          sessions.dirs = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [
              "/run/current-system/sw/share/wayland-sessions"
              "/run/current-system/sw/share/xsessions"
            ];
            description = "Where the login screen looks for session .desktop files.";
          };
        };
      };
    };

    fontPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ pkgs.noto-fonts ];
      defaultText = lib.literalExpression "[ pkgs.noto-fonts ]";
      description = "Fonts installed so `settings.font` resolves.";
    };

    login = {
      enable = lib.mkEnableOption "garde as the greetd greeter (under cage)";

      cagePackage = lib.mkPackageOption pkgs "cage" { };

      cageArgs = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "-s" ];
        description = "Arguments for cage (`-s` allows VT switching).";
      };

      cursor = {
        name = lib.mkOption {
          type = lib.types.str;
          default = "Posy_Cursor";
          description = "Cursor theme at the login screen.";
        };
        size = lib.mkOption {
          type = lib.types.ints.positive;
          default = 24;
          description = "Cursor size.";
        };
        package = lib.mkOption {
          type = lib.types.nullOr lib.types.package;
          default = pkgs.posy-cursors;
          defaultText = lib.literalExpression "pkgs.posy-cursors";
          description = "Package providing the cursor theme (null: rely on XCURSOR_PATH defaults).";
        };
      };
    };

    lock = {
      enable = lib.mkEnableOption "the garde lock screen (ext-session-lock-v1), started with the graphical session";

      cosmicLocker = lib.mkOption {
        type = lib.types.enum [ "keep" "exclude" "patch" ];
        default = if cfg.login.enable then "exclude" else "patch";
        defaultText = lib.literalExpression ''if login.enable then "exclude" else "patch"'';
        description = ''
          What to do with COSMIC's own lock screen, which cosmic-session starts in every session
          (the cosmic-greeter binary in locker mode) and which also locks on logind's Lock signal.
          Two lockers race for the lock; the loser is refused by cosmic-comp.

          - `exclude`: leave cosmic-greeter out of the system (environment.cosmic.excludePackages).
            cosmic-session logs one "failed to start cosmic-greeter" and carries on. Only works
            when cosmic-greeter is not also the login screen (it re-adds itself then).
          - `patch`: rebuild cosmic-session without the line that starts cosmic-greeter. Works
            with any login screen; costs a cosmic-session rebuild. Needs nixpkgs.overlays.
          - `keep`: leave COSMIC alone (then disable garde's lock daemon in COSMIC some other way).

          No effect when COSMIC isn't enabled.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      environment.systemPackages = [ cfg.package ];
      environment.etc."garde/config.toml".source = configFile;
      fonts.packages = cfg.fontPackages;
    }

    (lib.mkIf cfg.login.enable {
      assertions = [
        {
          assertion = !(config.services.displayManager.cosmic-greeter.enable or false);
          message = "services.garde.login replaces cosmic-greeter as the greetd greeter: disable services.displayManager.cosmic-greeter.";
        }
      ];
      services.greetd = {
        enable = true;
        settings.default_session = {
          command = "${greeter}";
          user = "greeter";
        };
      };
      # Last session picked, written by the greeter.
      systemd.tmpfiles.rules = [ "d /var/lib/garde 0755 greeter greeter - -" ];
      # The greeter needs a GPU and input like any compositor.
      hardware.graphics.enable = lib.mkDefault true;
    })

    (lib.mkIf cfg.lock.enable {
      security.pam.services.garde = { };

      systemd.user.services.garde-lock = {
        description = "garde lock screen (locks on logind Lock and before sleep)";
        wantedBy = [ "graphical-session.target" ];
        partOf = [ "graphical-session.target" ];
        after = [ "graphical-session.target" ];
        unitConfig.ConditionEnvironment = "WAYLAND_DISPLAY";
        serviceConfig = {
          ExecStart = "${lib.getExe cfg.package} lock --daemon";
          # A crash leaves the screen locked (compositor's job); the restarted daemon takes the
          # abandoned lock back over so there's a password prompt again.
          Restart = "on-failure";
          RestartSec = 1;
          # 3: compositor has no ext-session-lock-v1 (KWin). Nothing to do there.
          RestartPreventExitStatus = 3;
          SuccessExitStatus = 3;
        };
      };
    })

    (lib.mkIf (cfg.lock.enable && cosmicEnabled && cfg.lock.cosmicLocker == "exclude") {
      environment.cosmic.excludePackages = [ cosmicGreeterPkg ];
      # Intentional, documented above.
      services.desktopManager.cosmic.showExcludedPkgsWarning = lib.mkDefault false;
    })

    (lib.mkIf (cfg.lock.enable && cosmicEnabled && cfg.lock.cosmicLocker == "patch") {
      nixpkgs.overlays = [
        (_final: prev: {
          cosmic-session = prev.cosmic-session.overrideAttrs (old: {
            postPatch = (old.postPatch or "") + ''
              # garde: don't start cosmic-greeter's in-session locker.
              substituteInPlace src/main.rs --replace-fail \
                'start_component("cosmic-greeter", span, &process_manager, &env_vars).await;' \
                'let _ = span;'
            '';
          });
        })
      ];
    })
  ]);
}
