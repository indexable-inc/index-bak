{
  id = "claude-launch-ex";
  inRustWorkspace = true;
  # The NIF library comes from the shared cargo-unit workspace graph;
  # default.nix assembles the mix package (generated lib/ + priv/ + the
  # ExUnit suite) and runs the suite.
  packageSet = true;
  # Gate the suite as `checks.<system>.claude-launch-ex-run`.
  passthruTests = {
    prefix = "claude-launch-ex";
  };
}
