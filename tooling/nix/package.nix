{
  pkgs ? import <nixpkgs> {},
  fenix ? import (fetchTarball "https://github.com/nix-community/fenix/archive/main.tar.gz") {},
}: let
  inherit (pkgs) lib stdenv;
  rust-toolchain = (lib.importTOML ./../../rust-toolchain.toml).toolchain;
  complete-toolchain = fenix.fromToolchainName {
    name = rust-toolchain.channel;
    sha256 = "sha256-6eN/GKzjVSjEhGO9FhWObkRFaE1Jf+uqMSdQnb8lcB4=";
  };
  rustPlatform = pkgs.makeRustPlatform {
    inherit (complete-toolchain) cargo rustc;
  };
in
  rustPlatform.buildRustPackage rec {
    name = "zed-editor";
    version = "git";

    src = ./../..;

    nativeBuildInputs = with pkgs;
      [
        copyDesktopItems
        curl
        perl
        pkg-config
        protobuf
        rustPlatform.bindgenHook
      ]
      ++ lib.optionalString stdenv.isLinux [llvmPackages.clangUseLLVM llvmPackages.bintools mold]
      ++ lib.optionals stdenv.isDarwin [xcbuild.xcrun];

    buildInputs = with pkgs;
      [
        curl
        fontconfig
        freetype
        libgit2
        openssl
        sqlite
        zlib
        zstd
      ]
      ++ lib.optionals stdenv.isLinux [
        alsa-lib
        libxkbcommon
        wayland
        xorg.libxcb
        mold
      ]
      ++ lib.optionals stdenv.isDarwin (
        with darwin.apple_sdk.frameworks; [
          AppKit
          CoreAudio
          CoreFoundation
          CoreGraphics
          CoreMedia
          CoreServices
          CoreText
          Foundation
          IOKit
          Metal
          Security
          SystemConfiguration
          VideoToolbox
        ]
      );

    cargoLock = {
      lockFile = ./../../Cargo.lock;
      outputHashes = lib.importJSON ./pins.json;
    };

    cargoBuildFlags = [
      "--package=zed"
      "--package=cli"
    ];

    buildFeatures = ["gpui/runtime_shaders" "mimalloc"];

    RUSTFLAGS = "-C symbol-mangling-version=v0 --cfg tokio_unstable -C target-cpu=x86-64-v3 -C link-arg=-fuse-ld=mold";

    env = {
      ZSTD_SYS_USE_PKG_CONFIG = true;
      OPENSSL_NO_VENDOR = 1;
      FONTCONFIG_FILE = pkgs.makeFontsConf {
        fontDirectories = [
          "${src}/assets/fonts/zed-mono"
          "${src}/assets/fonts/zed-sans"
        ];
      };
    };

    # Using fenix seems to have broken the bindgen hook.
    postFixup = let
      dynlibs = with pkgs; buildInputs ++ [vulkan-loader];
    in
      lib.optionalString stdenv.isLinux (pkgs.lib.concatStringsSep "; " (builtins.map (b: "patchelf --add-rpath ${b.out}/lib $out/libexec/*") dynlibs));
    doCheck = false;

    checkFlags = lib.optionals stdenv.hostPlatform.isLinux [
      # Fails with "On 2823 Failed to find test1:A"
      "--skip=test_base_keymap"
      # Fails with "called `Result::unwrap()` on an `Err` value: Invalid keystroke `cmd-k`"
      # https://github.com/zed-industries/zed/issues/10427
      "--skip=test_disabled_keymap_binding"
    ];

    installPhase = ''
      runHook preInstall

      mkdir -p $out/bin $out/libexec
      cp target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/zed $out/libexec/zed-editor
      cp target/${stdenv.hostPlatform.rust.cargoShortTarget}/release/cli $out/bin/zed

      install -D ${src}/crates/zed/resources/app-icon@2x.png $out/share/icons/hicolor/1024x1024@2x/apps/zed.png
      install -D ${src}/crates/zed/resources/app-icon.png $out/share/icons/hicolor/512x512/apps/zed.png

      # extracted from https://github.com/zed-industries/zed/blob/v0.141.2/script/bundle-linux (envsubst)
      # and https://github.com/zed-industries/zed/blob/v0.141.2/script/install.sh (final desktop file name)
      (
        export DO_STARTUP_NOTIFY="true"
        export APP_CLI="zed"
        export APP_ICON="zed"
        export APP_NAME="Zed"
        export APP_ARGS="%U"
        mkdir -p "$out/share/applications"
        ${lib.getExe pkgs.envsubst} < "crates/zed/resources/zed.desktop.in" > "$out/share/applications/dev.zed.Zed.desktop"
      )

      runHook postInstall
    '';

    meta = with lib; {
      description = "High-performance, multiplayer code editor from the creators of Atom and Tree-sitter";
      homepage = "https://zed.dev";
      changelog = "https://github.com/zed-industries/zed/releases/tag/v${version}";
      license = licenses.gpl3Only;
      maintainers = with maintainers; [
        GaetanLepage
        niklaskorz
      ];
      mainProgram = "zed";
      platforms = platforms.all;
      # Currently broken on darwin: https://github.com/NixOS/nixpkgs/pull/303233#issuecomment-2048650618
      broken = stdenv.isDarwin;
    };
  }
