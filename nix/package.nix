{
  lib,
  rustPlatform,
  pkg-config,
  libxkbcommon,
  pam,
  xkeyboard_config,
}:

rustPlatform.buildRustPackage {
  pname = "garde";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # bindgenHook: pam-sys generates its bindings against libpam's headers.
  nativeBuildInputs = [ pkg-config rustPlatform.bindgenHook ];
  buildInputs = [ libxkbcommon pam ];

  # The keyboard tests compile real fr/us keymaps.
  preCheck = ''
    export XKB_CONFIG_ROOT=${xkeyboard_config}/share/X11/xkb
  '';

  meta = {
    description = "greetd login screen and Wayland (ext-session-lock-v1) lock screen";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "garde";
  };
}
