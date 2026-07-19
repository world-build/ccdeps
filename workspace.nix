{ pkgs, ... }:
{
  languages.rust = {
    enable = true;
    src = ./.;

    binaries.ccdeps = {
      member = "ccdeps-cli";
    };

    deps = [
      pkgs.gcc
      pkgs.clang
    ];

    # The deps layer trips over the root crate's [[bench]] target.
    depsLayer = false;
  };
}
