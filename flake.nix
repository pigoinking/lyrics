{
  description = "Realtime lyrics overlay for Cider 2 on Wayland/Sway";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        # Common build inputs
        buildInputs = with pkgs; [
          wayland
          libxkbcommon
          fontconfig
          freetype
        ];

        nativeBuildInputs = with pkgs; [
          pkg-config
        ];

        lyrics-overlay-unwrapped = pkgs.rustPlatform.buildRustPackage {
          pname = "lyrics-overlay";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          inherit buildInputs nativeBuildInputs;

          meta = with pkgs.lib; {
            description = "Realtime lyrics overlay for Cider 2";
            license = licenses.mit;
            platforms = platforms.linux;
          };
        };

        # Wrapper function to create configured overlay
        mkLyricsOverlay = {
          apiToken ? null,           # Cider API token (required)
          offsetMs ? 300,            # Lyrics time offset in ms
          topMargin ? 50,            # Distance from top of screen
          rightMargin ? 10,          # Distance from right edge
          width ? 600,               # Overlay width
          height ? 120,              # Overlay height (120 for multi-line support)
          fontSize ? 22.0,           # Font size
          allowArtistMismatch ? false, # Use lyrics from different artist
        }: let
          # The main overlay binary with config
          overlay = pkgs.writeShellScriptBin "lyrics-overlay" ''
            export LYRICS_OFFSET_MS="${toString offsetMs}"
            export LYRICS_TOP_MARGIN="${toString topMargin}"
            export LYRICS_RIGHT_MARGIN="${toString rightMargin}"
            export LYRICS_WIDTH="${toString width}"
            export LYRICS_HEIGHT="${toString height}"
            export LYRICS_FONT_SIZE="${toString fontSize}"
            ${if allowArtistMismatch then ''export LYRICS_ALLOW_ARTIST_MISMATCH="1"'' else ""}
            ${if apiToken != null then ''export CIDER_API_TOKEN="${apiToken}"'' else ""}
            exec ${lyrics-overlay-unwrapped}/bin/lyrics-overlay "$@"
          '';

          # Toggle script - starts if not running, toggles visibility if running
          toggle = pkgs.writeShellScriptBin "lyrics-overlay-toggle" ''
            SOCKET="/tmp/lyrics-overlay.sock"

            if ${pkgs.procps}/bin/pgrep -x "lyrics-overlay" > /dev/null; then
              # Running - send toggle signal via socket or SIGUSR1
              if [ -S "$SOCKET" ]; then
                echo "toggle" | ${pkgs.netcat}/bin/nc -U "$SOCKET" -N 2>/dev/null || true
              fi
              ${pkgs.procps}/bin/pkill -USR1 -x "lyrics-overlay" 2>/dev/null || true
            else
              # Not running - start it
              ${overlay}/bin/lyrics-overlay &
              disown
            fi
          '';
        in pkgs.symlinkJoin {
          name = "lyrics-overlay";
          paths = [ overlay toggle ];
        };

        # Default wrapper (no token, use env var)
        lyrics-overlay = mkLyricsOverlay {};

      in
      {
        packages = {
          default = lyrics-overlay;
          unwrapped = lyrics-overlay-unwrapped;
        };

        # Export the wrapper function for use in other flakes
        lib.mkLyricsOverlay = mkLyricsOverlay;

        devShells.default = pkgs.mkShell {
          buildInputs = buildInputs ++ nativeBuildInputs ++ [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.cargo-watch
            pkgs.netcat
          ];

          RUST_LOG = "debug";
        };
      });
}
