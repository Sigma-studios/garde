# garde

One Rust binary with two modes that look the same: the desktop's wallpaper, the user's name, one
password field, a caps-lock warning and the keyboard layout. There is no username field; the user
is fixed in the config.

- `garde login`: a [greetd](https://sr.ht/~kennylevinsen/greetd/) greeter, run under
  [cage](https://github.com/cage-kiosk/cage). Adds a session picker and shut down/reboot buttons (or F1/F2).
- `garde lock`: a Wayland lock screen on `ext-session-lock-v1`. `garde lock --daemon` locks on
  logind's `Lock` signal (`loginctl lock-session`, which COSMIC's idle timer, Super+Escape and
  Génie's Lock all use) and before sleep.

All drawing is on the CPU (tiny-skia, cosmic-text, wl_shm), with no GPU dependency, so it doesn't
care about the NVIDIA driver under cage. Nothing redraws unless there's input, the caret blinks or
the state changes.

## Usage

```
garde login [--config PATH]           # under cage: cage -s -- garde login
garde lock [--daemon] [--config PATH]
```

The config is `/etc/garde/config.toml` by default, or `$GARDE_CONFIG`. Exit codes:

| code | meaning |
| ---- | ------- |
| 0 | unlocked, or the session started |
| 1 | error, or another locker holds the lock (`finished`) |
| 2 | bad arguments |
| 3 | the compositor has no `ext-session-lock-v1` (KWin) |

## Config

```toml
user = "sigma"                              # the only user; shown, never typed
wallpaper = "/nix/store/…-wallpaper.png"   # same file as the desktop's
layouts = ["fr", "us"]                      # xkb layouts, first is default
numlock = true
dim = 0.35                                  # darkening behind the card, 0 = none
blur = 0                                    # blur radius behind the card, 0 = off
font = "Noto Sans"
# pam_service = "garde"                     # lock mode
# state_file = "/var/lib/garde/state.toml"  # login mode: last session picked
# xsession_wrapper = ["startx", "/usr/bin/env"]
[sessions]
dirs = ["/run/current-system/sw/share/wayland-sessions", "/run/current-system/sw/share/xsessions"]
```

The name shown is the GECOS full name, falling back to the login name. Unknown keys are rejected.

## Behaviour

- **Wallpaper**: scaled to fill each output (zoom and crop). `dim` and `blur` apply only behind
  the card, which gives it a frosted-glass look. A missing wallpaper falls back to a dark
  gradient; it never stops the locker.
- **Card**: drawn on the output under the pointer, else the top-left output. Other outputs show
  only the wallpaper. It holds the name and password field, and makes room for the caps-lock
  warning and messages only while they're shown, growing downwards, so the name and field never
  move. The session picker (login) and the layout pill sit in the bottom-left corner of that
  output.
- **Keyboard**: uses the compositor's keymap. Under cage, the Nix module sets
  `XKB_DEFAULT_LAYOUT` from `layouts`. garde always starts on the first layout. Super+Alt+K or a
  click on the layout pill cycles it locally, and a layout switch made by the compositor still wins.
  `numlock = true` turns num lock on locally (it flips the compositor's state, so the Num Lock key
  still toggles it); the keyboard LED may not follow. Caps lock is read from xkb's modifier state.
- **Enter on an empty field** does nothing (people press it to wake the screen), so it never
  costs a faillock attempt.
- **Password**: kept in a `Zeroizing<String>` with 512 bytes reserved up front, so typing never
  reallocates. Backspace wipes the removed bytes, and the field is wiped after every attempt.
  Copies garde can't reach are not wiped: the `CString` handed to libpam, and greetd's own buffers.
- **Messages**: "Wrong password" when PAM or greetd says nothing more. Otherwise their messages
  are shown verbatim: error messages first, else info messages. faillock's "The account is locked
  due to 3 failed logins. (10 minutes left to unlock)" is sent as info.
- **Login**: sessions come from `.desktop` files (`Name`, `Exec`, `DesktopNames`); `Hidden`,
  `NoDisplay` and failed `TryExec` entries are skipped. Pick with Left/Right or by clicking the
  arrows (or the name, for the next one). The picker defaults to the last session started, which is saved in the state file.
  StartSession passes `XDG_SESSION_TYPE`, `XDG_SESSION_DESKTOP` (the file's id) and
  `XDG_CURRENT_DESKTOP`. On failure garde shows the error, sends CancelSession, opens a new session
  for the same user and clears the field; it never exits on a wrong password. If an attempt fails
  without asking anything (a lock-out stack with `requisite`), garde waits for the next Enter
  instead of retrying in a loop. The shut down and reboot buttons in the
  bottom-right corner, or F1 and F2, ask for the same button or key again within 6 s, then call
  logind's PowerOff/Reboot.
- **Lock**: one lock surface per output, including outputs plugged in while locked. PAM runs on a
  worker thread with the status "Checking…". The only call to `unlock_and_destroy` follows a
  successful PAM check or logind's `Unlock`. Every error path exits or keeps the lock, and if
  garde dies the compositor keeps the session locked. `finished` (another locker won) makes
  `garde lock` exit 1, never unlocking.
- **Daemon**: holds a logind delay inhibitor so sleep waits for the lock, sets the session's
  `LockedHint`, and honours `Unlock` (as GNOME, KDE and cosmic-greeter do). It accepts these
  signals only as broadcasts from the current owner of `org.freedesktop.login1`; anything else on
  the system bus could otherwise send garde a forged `Unlock`. While locked it keeps a
  marker in `$XDG_RUNTIME_DIR`. If it crashes, systemd restarts it, and the new instance sees the
  marker and takes the abandoned lock back over, so you get a password prompt again rather than a
  blank screen.

## NixOS module

```nix
{
  inputs.garde.url = "path:/home/sigma/Projects/garde";   # or git+file://…

  outputs = { nixpkgs, garde, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        garde.nixosModules.default
        ({ pkgs, ... }: {
          services.garde = {
            enable = true;
            login.enable = true;          # garde is the greetd greeter (under cage)
            lock.enable = true;           # garde lock --daemon with the graphical session
            settings = {
              user = "sigma";
              wallpaper = ./wallpaper.png;  # copied to the store, readable by the greeter
              layouts = [ "fr" "us" ];
              numlock = true;
              dim = 0.35;
              blur = 0;
              font = "Noto Sans";
            };
            # lock.cosmicLocker = "exclude";   # default with login.enable, see below
          };
          # then drop services.displayManager.cosmic-greeter.enable
        })
      ];
    };
  };
}
```

Flake outputs: `packages.<system>.{default,garde}`, `overlays.default`, `nixosModules.default`,
`checks.<system>.garde` (the unit tests) and `devShells.<system>.default`, for `x86_64-linux` and
`aarch64-linux`, with nixpkgs pinned to `1c89083e451e891152365b799595cf7790a43325`.

| option | default | effect |
| ------ | ------- | ------ |
| `services.garde.enable` | `false` | installs garde, writes `/etc/garde/config.toml`, adds `fontPackages` |
| `services.garde.package` | this flake's | |
| `services.garde.settings` | | rendered to `/etc/garde/config.toml` (freeform TOML; typed: `user`, `wallpaper`, `layouts`, `numlock`, `dim`, `blur`, `font`, `sessions.dirs`) |
| `services.garde.fontPackages` | `[ pkgs.noto-fonts ]` | so `settings.font` resolves |
| `services.garde.login.enable` | `false` | greetd `default_session` = cage running garde as the `greeter` user, `XKB_DEFAULT_LAYOUT`/`VARIANT` from `layouts`, Posy cursor, `/var/lib/garde` owned by `greeter`; asserts cosmic-greeter is off |
| `services.garde.login.cagePackage` / `cageArgs` | `pkgs.cage` / `[ "-s" ]` | |
| `services.garde.login.cursor.{name,size,package}` | `Posy_Cursor`, `24`, `pkgs.posy-cursors` | Posy is unfree (CC BY-NC): needs `allowUnfree`, or set `package = null` |
| `services.garde.lock.enable` | `false` | `security.pam.services.garde`, and the `garde-lock` systemd user unit (wanted by `graphical-session.target`, `Restart=on-failure`, exit 3 counts as success and isn't restarted) |
| `services.garde.lock.cosmicLocker` | `"exclude"` with login, else `"patch"` | what to do with COSMIC's in-session locker, see below |

### COSMIC's own locker

cosmic-session starts `cosmic-greeter` in every session (`src/main.rs:349-350` at `epoch-1.11.0`,
found via `$PATH`, no arguments). For a normal user the binary runs as the locker. It subscribes to
logind's `Lock`/`Unlock`/`PrepareForSleep` itself, so it competes with garde for every
`loginctl lock-session`. cosmic-comp lets only one lock exist at a time and sends the other client
`finished`. cosmic-greeter ignores `finished` (a TODO in `locker.rs`) and gets stuck, so whichever
starts locking first wins. There is no setting or environment variable to turn the locker off.
cosmic-session restarts it on a crash, but not when it exits 0, and logs only once if the binary is
missing. `lock.cosmicLocker` therefore offers:

- `exclude`: `environment.cosmic.excludePackages = [ cosmic-greeter ]`, which also silences
  COSMIC's warning about excluding a core package. There's no rebuild; cosmic-session logs
  "failed to start cosmic-greeter" once. This only works when cosmic-greeter isn't the login
  screen, which is the case with `login.enable`.
- `patch`: an overlay rebuilds cosmic-session 1.11 with that `start_component("cosmic-greeter", …)`
  call removed (`substituteInPlace --replace-fail`, so a future source change fails the build
  loudly). This works with any login screen, at the cost of a cosmic-session rebuild. It needs
  `nixpkgs.overlays`, so it doesn't work with an externally supplied `nixpkgs.pkgs`.
- `keep`: leave COSMIC alone.

Super+Escape, the idle timer (`cosmic-idle` runs the `LockScreen` action, which is
`loginctl lock-session`) and Génie's Lock all go through logind, so garde receives every one of
them.

### Plasma

KWin 6.8 doesn't implement `ext-session-lock-v1`: kwin master (6.8.80) has no such code and no
open merge request. Plasma locks through kscreenlocker, which runs inside KWin and talks to its
greeter over a private socket. So Plasma keeps its own lock screen. In a Plasma session
`garde lock --daemon` exits 3 ("nothing to do"), and the user unit treats that as success and
doesn't restart it. `garde login` works for starting Plasma sessions.

## Development

```
nix develop            # cargo, clippy, rustfmt, XKB_CONFIG_ROOT for the keymap tests
cargo test
nix build              # also runs the tests
```

The unit tests cover session file parsing and Exec quoting, state persistence, the greetd message
flow against a mock socket (success, wrong password and retry, faillock lock-out in its `required`
and `requisite` forms), layout cycling, the caps-lock and num-lock logic on real `fr,us` keymaps,
display-name lookup, password-buffer hygiene, card geometry and hit-testing, and the
blur/scaling/blit helpers.
