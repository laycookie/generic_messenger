{
  description = "Front-end for chat backends";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";  # Specify the Nixpkgs version
	rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url  = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
  let
    system = "x86_64-linux";

	cmake_3_24_3_pkgs = import (builtins.fetchGit {
         # Descriptive name to make the store path easier to identify
         name = "cmake_3_24_3";
         url = "https://github.com/NixOS/nixpkgs/";
         ref = "refs/heads/nixpkgs-unstable";
         rev = "55070e598e0e03d1d116c49b9eff322ef07c6ac6";
    }) { inherit system; };
	overlays = [ 
		(import rust-overlay)
		(final: prev: {
			cmake_3_24_3 = cmake_3_24_3_pkgs.cmake;
		})
	];
    pkgs = import nixpkgs {
		inherit system overlays;
	};
  in
  {
		devShells.${system} = {
			default = pkgs.mkShell.override { stdenv = pkgs.clangStdenv; } {
    		    packages = with pkgs; [
					nodejs

				  rust-bin.nightly.latest.default
				  # rust-bin.stable.latest.default
    		      # cargo
    		      # rustc
    		      rust-analyzer
				  cargo-modules
    		      # rustfmt

				  cargo-expand

				  cmake_3_24_3

				  alsa-lib
				  libopus

				  fontconfig

				  openssl
				  pkg-config

				  # Build deps for game-networking-sockets-sys (compiles Valve's GNS):
				  # protoc + libprotobuf and abseil. See interface/steam/VOICE_PROTOCOL.md.
				  protobuf
				  abseil-cpp
    		    ];
				LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
					pkgs.libxkbcommon
					pkgs.wayland
					pkgs.vulkan-loader
				];

				# bindgen (game-networking-sockets-sys) needs libclang to parse headers.
				LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

    		    # RUST_BACKTRACE = "full";
				
				# Wayland
    		    # WINIT_UNIX_BACKEND = "wayland";
    		    
				# X11/Xwayland
				# WINIT_UNIX_BACKEND = "x11";
				# WAYLAND_DISPLAY="";
    		};
		};
	};
}
