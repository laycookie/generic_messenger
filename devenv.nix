{ pkgs, lib, config, ... }:
{
  languages.rust = {
    enable = true;
    # Keep Rust version, profile, and components synchronized with CI/local Rustup.
    toolchainFile = ./rust-toolchain.toml;
    clangLinker.enable = true;
  };

	packages = with pkgs; [
		rust-analyzer
		cargo-modules

		cargo-expand
		nasm

		alsa-lib
		dbus
		libopus
		linuxHeaders
		pipewire

		fontconfig

		openssl
		pkg-config
	];

  env = {
    CC = "${pkgs.clang}/bin/clang";
    CXX = "${pkgs.clang}/bin/clang++";
    LD_LIBRARY_PATH = lib.makeLibraryPath [
      pkgs.libxkbcommon
      pkgs.wayland
      pkgs.vulkan-loader
    ];
    LIBCLANG_PATH = lib.makeLibraryPath [ pkgs.libclang.lib ];
    BINDGEN_EXTRA_CLANG_ARGS = lib.concatStringsSep " " [
      "-isystem ${pkgs.glibc.dev}/include"
      "-isystem ${pkgs.linuxHeaders}/include"
    ];
  };
}
