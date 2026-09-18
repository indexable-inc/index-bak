{
  id = "nix-tui";
  packageSet = true;
  flake = true;
  # A terminal frontend nobody composes into another derivation; it is run,
  # not depended on. Unlike the server, which the overlay exposes so other
  # packages can wrap it.
  overlay = false;
  inRustWorkspace = true;
  passthruTests = true;
}
