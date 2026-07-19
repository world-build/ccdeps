# ccdeps is a world repo: the workspace declares its Rust targets in
# workspace.nix, and pins community.world for languages.rust plus nixpkgs.
# The dev loop stays native cargo; the hermetic loop builds the ccdeps-cli
# binary offline from Cargo.lock's own checksums (no git deps to vendor).
{ world, system }:
let
  pins = world.pins ./.world/pins.json;
in
world.new {
  version = 0;
  src = ./.;
  inherit pins;
  pkgs = import pins.nixpkgs { inherit system; };
  worlds = [ pins.community ];
}
