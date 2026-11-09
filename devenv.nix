{
  pkgs,
  config,
  multiverse,
  ...
}:

{
  dotenv.enable = true;

  packages = with pkgs; [
    just

    lld
    cargo-audit
    cargo-deny
    cargo-dist
    cargo-release
    cargo-watch

    # the native tests build atuin-client, which links OpenSSL
    openssl
    pkg-config

    # The stock client for the CLI smoke test (scripts/smoke.sh): the upstream version in Cargo.toml.
    multiverse.atuin."18.24.4"
  ];

  # worker-build and wasm-bindgen at the versions in Cargo.lock.
  worker-build = {
    enable = true;
    cargoLock = ./Cargo.lock;
  };

  languages = {
    rust = {
      enable = true;
      # The toolchain the releases are built with.
      toolchainFile = ./rust-toolchain.toml;
    };

    # wrangler (deploy/package-lock.json), for wrangler dev and the end-to-end tests: installed into
    # deploy/node_modules on entering the shell.
    javascript = {
      enable = true;
      package = pkgs.nodejs_22;
      directory = "${config.devenv.root}/deploy";
      npm.enable = true;
      npm.install.enable = true;
    };
  };
}
