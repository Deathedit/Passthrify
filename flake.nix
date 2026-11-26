{
  description = "Passthrify: low-latency capture card viewer with instant replay";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;

      # ffmpeg-sys-next 8.1 needs FFmpeg 8 headers (the default ffmpeg is newer). nixpkgs builds it
      # with nvenc and cuda-llvm (scale_cuda / colorspace_cuda).
      ffmpeg = pkgs.ffmpeg_8;
      # FFMPEG_DIR layout ffmpeg-sys-next expects: include/ + lib/ in one place.
      ffmpegDir = pkgs.symlinkJoin { name = "ffmpeg-dir"; paths = [ ffmpeg.dev ffmpeg.lib ]; };

      # Loaded at runtime with dlopen (winit, wgpu, CUDA/NVENC via the driver).
      runtimeLibs = with pkgs; [
        vulkan-loader
        wayland
        libxkbcommon
        libx11
        libxcursor
        libxrandr
        libxi
      ];
      # NixOS puts the NVIDIA driver libraries (libcuda, libnvidia-encode, the Vulkan ICD) here.
      driverLibPath = "/run/opengl-driver/lib";

      nativeBuildInputs = with pkgs; [ pkg-config rustPlatform.bindgenHook makeWrapper ];
      buildInputs = [ ffmpeg pkgs.pipewire ] ++ runtimeLibs;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        inherit buildInputs;
        nativeBuildInputs = nativeBuildInputs ++ (with pkgs; [ cargo rustc clippy rustfmt ]);
        # Override .cargo/config.toml's Windows paths ([env] never beats the environment).
        FFMPEG_DIR = ffmpegDir;
        LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs + ":${driverLibPath}";
      };

      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "passthrify";
        version = (lib.importTOML ./Cargo.toml).package.version;
        src = lib.cleanSource ./.;
        cargoLock.lockFile = ./Cargo.lock;
        inherit nativeBuildInputs buildInputs;
        FFMPEG_DIR = ffmpegDir;
        PASSTHRIFY_REPO = "Deathedit/Passthrify";
        # Tests that need a capture card or GPU are skipped by the test suite itself; the rest run.
        postInstall = ''
          install -Dm644 assets/passthrify.desktop $out/share/applications/passthrify.desktop
          install -Dm644 assets/logo/passthrify-mark.svg $out/share/icons/hicolor/scalable/apps/passthrify.svg
          wrapProgram $out/bin/passthrify \
            --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}:${driverLibPath}
        '';
        meta = {
          description = "Low-latency capture card viewer with instant replay, recording and streaming";
          mainProgram = "passthrify";
          platforms = [ system ];
        };
      };
    };
}
