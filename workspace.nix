{ pkgs, ... }:
{
  languages.rust = {
    enable = true;
    src = ./.;

    # `ccdeps` is the shipped binary; it lives in the ccdeps-cli member.
    # The ccdeps library crate rides along as a workspace member (enix
    # consumes it directly as a rev-pinned git dependency, not from here).
    binaries.ccdeps = {
      member = "ccdeps-cli";
    };

    # Build each target from full source rather than a stubbed deps layer.
    # The dep closure is small, so the shared layer saves little, and the
    # stubbed layer trips over the root crate's [[bench]] target.
    depsLayer = false;

    # tests/conformance.rs diffs our output against real gcc/clang, so put them
    # on the shell and the rust.* tasks. The tests skip the comparison when a
    # compiler is missing, so this only sharpens the check.
    deps = [
      pkgs.gcc
      pkgs.clang
    ];
  };
}
