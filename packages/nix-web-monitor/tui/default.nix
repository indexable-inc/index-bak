{
  ix,
  lib,
  ...
}:
ix.cargoUnit.selectBinaryWithTests ix.rustWorkspace.units {
  binary = "nix-tui";
  meta = {
    description = "Terminal frontend for nix-web-monitor: live build table, builder host, and in-flight dependency DAG";
    license = lib.licenses.mit;
    mainProgram = "nix-tui";
  };
}
