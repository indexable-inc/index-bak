defmodule ClaudeLaunchTest do
  @moduledoc """
  The Elixir half of the typed launcher (ENG-11979): build the config as a
  struct, launch a child, and read `--output-format stream-json` back as
  typed events.

  The child is a stub `claude` this suite writes, not the real CLI, which
  needs credentials no build sandbox has. Everything asserted here belongs
  to the binding: the record round-trip, the argv rendering, the error
  variants, and the event stream's shape. The `:e2e` test at the bottom
  drives the real binary and is excluded unless `CLAUDE_LAUNCH_E2E=1`.
  """

  # Not async: each test spawns OS processes and writes an executable.
  use ExUnit.Case, async: false

  alias ClaudeLaunch.Block
  alias ClaudeLaunch.ClaudeError
  alias ClaudeLaunch.Config
  alias ClaudeLaunch.Event
  alias ClaudeLaunch.Features
  alias ClaudeLaunch.Init
  alias ClaudeLaunch.McpServer
  alias ClaudeLaunch.Outcome

  @init ~S({"type":"system","subtype":"init","session_id":"fake-session","model":"stub","permissionMode":"plan","cwd":"/","tools":["Read"],"claude_code_version":"0.0.0"})
  @assistant ~S({"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}})
  @result ~S({"type":"result","subtype":"success","is_error":false,"result":"hi","num_turns":1,"duration_ms":9,"duration_api_ms":4,"total_cost_usd":0.5,"session_id":"fake-session"})

  defp echo(lines), do: Enum.map_join(lines, "\n", &"printf '%s\\n' '#{&1}'")

  # A stand-in `claude` that runs `body` and ignores every argument.
  defp stub(body) do
    dir = Path.join(System.tmp_dir!(), "claude-launch-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    path = Path.join(dir, "claude")
    File.write!(path, "#!/bin/sh\n" <> body <> "\n")
    File.chmod!(path, 0o755)
    on_exit(fn -> File.rm_rf!(dir) end)
    path
  end

  defp config(path, overrides \\ []) do
    struct!(%{ClaudeLaunch.default_config() | prompt: "anything", bin: path}, overrides)
  end

  defp index_server do
    %McpServer{
      name: "index",
      transport: "stdio",
      command: "ix-mcp-ex",
      args: [],
      env: %{},
      url: "",
      headers: %{}
    }
  end

  defp flag_value(argv, flag) do
    case Enum.find_index(argv, &(&1 == flag)) do
      nil -> nil
      index -> Enum.at(argv, index + 1)
    end
  end

  describe "the config is a struct" do
    test "default_config/0 hands back one filled in" do
      assert %Config{
               prompt: nil,
               output_format: "stream-json",
               input_format: "text",
               session_mode: "new",
               mcp_policy: "inherit",
               permission_mode: nil,
               features: %Features{
                 strict_protocol: true,
                 session_persistence: true,
                 builtin_agents: true,
                 skip_permissions: false
               }
             } = ClaudeLaunch.default_config()
    end

    test "a config built as a struct renders the argv" do
      config = %{
        ClaudeLaunch.default_config()
        | prompt: "say hi",
          model: "opus",
          permission_mode: "plan",
          tools: [],
          mcp_policy: "none"
      }

      assert {:ok, ["claude", "-p", "say hi" | rest]} = ClaudeLaunch.argv(config)
      assert flag_value(rest, "--model") == "opus"
      assert flag_value(rest, "--permission-mode") == "plan"
      assert flag_value(rest, "--tools") == ""
      assert flag_value(rest, "--mcp-config") == ~S({"mcpServers":{}})
      assert "--strict-mcp-config" in rest
      # stream-json output is refused by the CLI without it.
      assert "--verbose" in rest
    end

    test "a launcher's own arguments render before the print flag" do
      # `bin` can be a wrapper that execs claude itself; the kernel's loom
      # runner passes a remote-exec wrapper the VM name and working dir.
      config = %{
        ClaudeLaunch.default_config()
        | prompt: "hi",
          bin: "remote-claude",
          launcher_args: ["vm-1", "/work"]
      }

      assert {:ok, ["remote-claude", "vm-1", "/work", "-p", "hi" | _]} =
               ClaudeLaunch.argv(config)
    end

    test "an empty tool list is not an absent one" do
      absent = %{ClaudeLaunch.default_config() | prompt: "hi"}
      assert {:ok, argv} = ClaudeLaunch.argv(absent)
      refute "--allowedTools" in argv

      assert {:ok, argv} = ClaudeLaunch.argv(%{absent | allowed_tools: []})
      assert flag_value(argv, "--allowedTools") == ""
    end

    test "turning built-in agents off is an env entry, not a flag" do
      config = %{ClaudeLaunch.default_config() | prompt: "hi"}
      config = %{config | features: %{config.features | builtin_agents: false}}
      assert {:ok, env} = ClaudeLaunch.env(config)
      assert env["CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"] == "1"
    end

    test "an unknown permission mode is refused with the accepted set" do
      config = %{ClaudeLaunch.default_config() | prompt: "hi", permission_mode: "yolo"}

      assert {:error, %ClaudeError{variant: :config, message: message}} =
               ClaudeLaunch.argv(config)

      assert message =~ "bypassPermissions"
    end

    test "permission_modes/0 is the discoverable set" do
      # The set is a Rust enum that flattens to String at this boundary
      # (ENG-11981), so it has to be reachable some other way.
      assert "acceptEdits" in ClaudeLaunch.permission_modes()
      assert length(ClaudeLaunch.permission_modes()) == 6
    end

    test "kernel_only/1 composes the three flags that mean one MCP server" do
      # --tools "" is the existence axis and --allowedTools is the approval
      # axis; this preset touches the first only.
      {:ok, config} = ClaudeLaunch.kernel_only(index_server())
      assert {:ok, argv} = ClaudeLaunch.argv(%{config | prompt: "hi"})

      assert flag_value(argv, "--tools") == ""
      assert "--strict-mcp-config" in argv
      refute "--allowedTools" in argv

      assert flag_value(argv, "--mcp-config") ==
               ~S({"mcpServers":{"index":{"args":[],"command":"ix-mcp-ex","env":{},"type":"stdio"}}})

      # The CLI's own subagent types would be a second spawn path under the
      # star topology, so the preset closes them too.
      refute config.features.builtin_agents
      assert {:ok, env} = ClaudeLaunch.env(%{config | prompt: "hi"})
      assert env["CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"] == "1"
    end

    test "an http mcp server renders its url" do
      server = %McpServer{
        name: "exa",
        transport: "http",
        command: "",
        args: [],
        env: %{},
        url: "https://mcp.exa.ai/mcp",
        headers: %{}
      }

      {:ok, config} = ClaudeLaunch.kernel_only(server)
      assert {:ok, argv} = ClaudeLaunch.argv(%{config | prompt: "hi"})

      assert flag_value(argv, "--mcp-config") ==
               ~S({"mcpServers":{"exa":{"headers":{},"type":"http","url":"https://mcp.exa.ai/mcp"}}})
    end

    test "an mcp server missing the field its transport needs is refused" do
      assert {:error, %ClaudeError{variant: :config, message: message}} =
               ClaudeLaunch.kernel_only(%{index_server() | command: ""})

      assert message =~ "needs a `command`"

      assert {:error, %ClaudeError{variant: :config, message: message}} =
               ClaudeLaunch.kernel_only(%{index_server() | transport: "carrier-pigeon"})

      assert message =~ "stdio, http, sse"
    end

    test "a prompt under stream-json input is refused before anything spawns" do
      config = %{ClaudeLaunch.default_config() | prompt: "hi", input_format: "stream-json"}
      assert {:error, %ClaudeError{variant: :config}} = ClaudeLaunch.events(config)
    end
  end

  describe "the event stream" do
    test "yields typed events and always ends with exited" do
      path = stub(echo([@init, @assistant, @result]))
      assert {:ok, stream} = ClaudeLaunch.events(config(path))
      events = Enum.to_list(stream)

      assert Enum.map(events, & &1.kind) == ["init", "assistant", "result", "exited"]

      assert [
               %Event{
                 kind: "init",
                 session_id: "fake-session",
                 init: %Init{model: "stub", claude_code_version: "0.0.0", tools: ["Read"]}
               },
               %Event{kind: "assistant", blocks: [%Block{kind: "text", text: "hi"}]},
               %Event{kind: "result", outcome: %Outcome{text: "hi", is_error: false}},
               %Event{kind: "exited", exit_code: 0}
             ] = events
    end

    test "run/1 drives it to the terminal result" do
      path = stub(echo([@init, @assistant, @result]))

      assert {:ok, %Outcome{text: "hi", session_id: "fake-session", num_turns: 1}} =
               ClaudeLaunch.run(config(path))
    end

    test "an event kind the launcher does not model stops a strict run" do
      path = stub(echo([@init, ~S({"type":"newly_invented"}), @result]))

      assert {:error, %ClaudeError{variant: :protocol, message: message}} =
               ClaudeLaunch.run(config(path))

      assert message =~ "newly_invented"
      # The message says how stale the mirror is, so a report of this does
      # not need someone to go and look it up.
      assert message =~ "2.1.220"
    end

    test "a relaxed run carries on past it" do
      path = stub(echo([@init, ~S({"type":"newly_invented"}), @result]))
      config = config(path)
      config = %{config | features: %{config.features | strict_protocol: false}}
      assert {:ok, %Outcome{text: "hi"}} = ClaudeLaunch.run(config)
    end

    test "a child that dies without a result reports its exit and stderr" do
      path = stub("echo 'error: unknown option --nonsense' >&2\nexit 3")

      assert {:error, %ClaudeError{variant: :exited, message: message}} =
               ClaudeLaunch.run(config(path))

      assert message =~ "exit 3"
      assert message =~ "unknown option --nonsense"
    end

    test "a missing executable is a spawn error, not a crashed scheduler" do
      config = config("/definitely/not/here/claude")
      assert {:error, %ClaudeError{variant: :spawn}} = ClaudeLaunch.run(config)
    end

    test "nothing is produced without demand" do
      path = stub(echo([@init, @assistant, @result]))
      assert {:ok, handle} = ClaudeLaunch.events_stream(config(path))
      refute_receive {:unibind_stream, _, _}, 200
      :ok = ClaudeLaunch.stream_demand(handle, 1)
      assert_receive message, 5_000
      assert {:item, %Event{kind: "init"}} = ClaudeLaunch.stream_message(handle, message)
    end
  end

  describe "the real CLI" do
    @tag :e2e
    test "a claude -p run comes back as typed events" do
      config = %{
        ClaudeLaunch.default_config()
        | prompt: "Reply with exactly: hello from unibind",
          # No built-in tools and no MCP servers: the run can read nothing
          # and change nothing. `tools: []` alone leaves MCP tools behind,
          # which is why the pair is the safe posture.
          tools: [],
          mcp_policy: "none"
      }

      assert {:ok, stream} = ClaudeLaunch.events(config)
      events = Enum.to_list(stream)
      kinds = Enum.map(events, & &1.kind)

      assert "init" in kinds
      assert "result" in kinds
      assert List.last(kinds) == "exited"

      init = Enum.find(events, &(&1.kind == "init"))

      # `tools: []` empties the built-in set, which is what this asserts.
      # Anything still listed is an MCP tool, a separate axis: on a machine
      # where `claude` is a wrapper script that appends its own
      # `--mcp-config=<path>` after ours, `--strict-mcp-config` allows every
      # config on the command line rather than only the first, so
      # `mcp_policy: "none"` cannot promise an empty MCP set through a
      # wrapper. Observed against the nix-wrapped claude on 2026-08-02.
      builtins = Enum.reject(init.init.tools, &String.starts_with?(&1, "mcp__"))
      assert builtins == []

      result = Enum.find(events, &(&1.kind == "result"))
      assert result.outcome.text =~ "hello"
      refute result.outcome.is_error
    end
  end
end
