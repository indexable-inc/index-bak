# support/eventually.exs is staged next to this file by the ex target of
# unibind.build (packages/unibind/nix/ex.nix); the suite only runs inside
# the assembled mix package, never straight from this source tree.
Code.require_file("support/eventually.exs", __DIR__)

# The `e2e` tests drive the real `claude` binary, which needs credentials no
# build sandbox has. Run them by hand with a working login:
#
#     CLAUDE_LAUNCH_E2E=1 mix test --include e2e
excluded = if System.get_env("CLAUDE_LAUNCH_E2E") == "1", do: [], else: [:e2e]
ExUnit.start(exclude: excluded)
