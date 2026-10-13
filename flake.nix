{
  description = "mmsearch - multimodal search overlay for Wayland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;
        toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        rustPlatform = pkgs.makeRustPlatform { cargo = toolchain; rustc = toolchain; };
        runtimeLibs = with pkgs; [ vulkan-loader wayland libxkbcommon alsa-lib ];
        runtimeBins = with pkgs; [ ffmpeg-headless poppler-utils slurp wl-clipboard xdg-utils curl util-linux systemd libnotify ];

        mmsearch = rustPlatform.buildRustPackage {
          pname = "mmsearch";
          version = (lib.importTOML ./Cargo.toml).package.version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./systemd ];
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            # The winit fork; the hash covers every crate of that git checkout.
            outputHashes."dpi-0.1.2" = "sha256-7DW0eaqJ5S0ixl4aio+cAE8qnq77tT9yzbemJJOGDX0=";
            # The llama-cpp-rs fork; fetchgit includes the llama.cpp submodule.
            outputHashes."llama-cpp-sys-2-0.1.160" = "sha256-6ZbZiecVHx43adJ3jxqyNv151sxQs2adOYYIkkmo1xs=";
          };

          nativeBuildInputs = with pkgs; [ cmake pkg-config shaderc git makeWrapper rustPlatform.bindgenHook ];
          buildInputs = with pkgs; [ vulkan-headers spirv-headers ] ++ runtimeLibs;
          # cmake is only for llama.cpp inside the cargo build, not for this package.
          dontUseCmakeConfigure = true;
          doCheck = false;

          postInstall = ''
            wrapProgram $out/bin/mmsearch \
              --prefix PATH : ${lib.makeBinPath runtimeBins} \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
            install -Dm644 systemd/mmsearch.service $out/lib/systemd/user/mmsearch.service
            substituteInPlace $out/lib/systemd/user/mmsearch.service --replace-fail @out@ $out
          '';

          meta = {
            description = "Multimodal search overlay for Wayland";
            license = lib.licenses.mit;
            mainProgram = "mmsearch";
            platforms = lib.platforms.linux;
          };
        };
      in
      {
        packages.default = mmsearch;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ toolchain pkg-config cmake clang shaderc ];
          buildInputs = with pkgs; [ vulkan-headers spirv-headers ] ++ runtimeLibs;
          packages = runtimeBins ++ [ pkgs.grim pkgs.jq ];

          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
        };
      });
}
