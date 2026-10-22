{
  description = "Browse and search AI assistant conversation history from the terminal.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
    }:
    let
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      eachSystem = nixpkgs.lib.genAttrs (import systems);
    in
    {
      packages = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            inherit (cargoToml.package) version;

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [ pkgs.pkg-config ];

            buildInputs =
              [
                pkgs.onnxruntime
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                pkgs.apple-sdk
                pkgs.libiconv
              ];

            ORT_LIB_PATH = "${pkgs.lib.getLib pkgs.onnxruntime}/lib";
            ORT_PREFER_DYNAMIC_LINK = "1";

            # Some tests require filesystem access not available in Nix sandbox
            doCheck = false;

            meta = with pkgs.lib; {
              description = "Browse and search AI assistant conversation history from the terminal.";
              homepage = "https://github.com/astral303/rearview";
              license = licenses.mit;
              mainProgram = "rearview";
            };
          };
        }
      );

      apps = eachSystem (
        system: {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/rearview";
          };
        }
      );

      devShells = eachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              rust-analyzer
              clippy
              rustfmt
            ];
            RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
          };
        }
      );
    };
}
