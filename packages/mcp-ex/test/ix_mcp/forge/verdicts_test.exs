defmodule IxMcp.Forge.VerdictsTest do
  use ExUnit.Case, async: false

  alias IxMcp.Forge.Verdicts

  # The fixtures are structurally faithful to the CI reconciler's real run
  # records -- every key, every type, and the gate verdict block's exact
  # layout, verified against two real runs on the forge host -- with all free
  # text invented. This tree is public and a real record carries commit
  # descriptions and host internals, so "it holds no secrets" is not the
  # binding test; the tier is.
  @fixtures Path.expand("../../fixtures", __DIR__)

  defp record(name) do
    @fixtures
    |> Path.join("forge-run-#{name}.json")
    |> File.read!()
    # The reader collapses each pretty-printed record to one line, which is
    # the shape fetch/3 parses. Doing it here keeps the fixtures readable as
    # files while testing the real wire shape.
    |> String.replace("\n", "")
  end

  defp reader(lines) do
    fn _since -> {:ok, Enum.join(lines, "\n") <> "\n"} end
  end

  defp fetch(lines, limit \\ 20) do
    {:ok, state} = Verdicts.init(target: "/fixture/runs", read: reader(lines))
    Verdicts.fetch(state, ~U[2026-08-12 00:00:00Z], limit)
  end

  # Distinct timestamps per record, so the expected order is the one the feed
  # promises (oldest first) rather than whatever a stable sort happened to
  # leave equal keys in.
  defp capped_records(prefix, range) do
    for index <- range do
      record("passed")
      |> JSON.decode!()
      |> Map.merge(%{
        "run_id" => "#{prefix}#{index}-1786546496920",
        "updated_at_ms" => 1_786_546_496_920 + index
      })
      |> JSON.encode!()
    end
  end

  describe "init/1" do
    test "no configured forge means the feed never starts" do
      assert :ignore = Verdicts.init([])
    end

    test "an explicit off switch wins over a configured forge" do
      System.put_env("IX_MCP_FORGE_WATCH", "0")
      on_exit(fn -> System.delete_env("IX_MCP_FORGE_WATCH") end)

      assert :ignore = Verdicts.init(target: "/fixture/runs", read: fn _since -> {:ok, ""} end)
    end

    test "a local target naming no directory is a misconfiguration, not a sleeping host" do
      assert :ignore = Verdicts.init(target: "/fixture/definitely/absent")
    end

    # The target is interpolated into a shell script, so its charset is the
    # security boundary. Each of these would be a command injection if the
    # regex were dropped, and the feed must refuse rather than sanitize.
    test "a target carrying shell metacharacters is refused" do
      for hostile <- [
            "/runs; rm -rf /",
            "/runs' -o -exec sh -c 'id",
            "/runs$(id)",
            "/runs`id`",
            "/runs\nid",
            "relative/runs",
            "root@host:relative"
          ] do
        assert :ignore = Verdicts.init(target: hostile),
               "expected #{inspect(hostile)} to be refused as a target"
      end
    end

    # `host:/dir` is what an ssh config alias looks like, and the first live
    # land of IxMcp.Stdlib.Forge was refused for having no `user@` part.
    test "a bare hostname with no user part is a normal ssh target" do
      assert {:ok, %{target: %{host: "fixture-host", dir: "/fixture/runs"}}} =
               Verdicts.init(
                 target: "fixture-host:/fixture/runs",
                 read: fn _s -> {:ok, ""} end
               )
    end

    test "a well-formed remote target is accepted" do
      assert {:ok, %{target: %{host: "root@fixture-host", dir: "/fixture/runs"}}} =
               Verdicts.init(
                 target: "root@fixture-host:/fixture/runs",
                 read: fn _s -> {:ok, ""} end
               )
    end
  end

  describe "fetch/3" do
    test "a passed run becomes a pass item with twelve-hex prefixes and a run duration" do
      assert {:ok, [item], false, _state} = fetch([record("passed")])

      assert item.id == "1f2e3d4c5b6a-1786546496920"
      assert item.verdict == :pass
      assert item.commit_id == "1f2e3d4c5b6a"
      assert item.change_id == "0a1b2c3d4e5f"
      assert item.target == "main"
      assert item.duration_ms == 309_000
      # A pass needs no garnish, and asking the log for it would be work
      # nobody reads.
      assert item.failed_stages == []
      assert item.tolerated == []
      assert item.log == nil
    end

    test "a failed run carries the failing stages, the already-red set, and the log path" do
      assert {:ok, [item], false, _state} = fetch([record("failed")])

      assert item.verdict == :fail
      assert item.commit_id == "9d8c7b6a5f4e"
      assert item.duration_ms == 388_000
      # `incr` appears twice in the gate's own output (stage table and
      # summary); one failing stage is one name.
      assert item.failed_stages == ["incr"]
      assert item.tolerated == ["fixture-tolerated-check", "fixture-second-tolerated"]
      assert item.log == "/fixture/logs/gate-20260101T000000Z-2.log"
    end

    # The instrument has to be able to say no as well as yes: a run that is
    # still building is written to the same directory with the same keys, and
    # announcing it would be a verdict that has not happened -- which is the
    # exact false-landing this feed exists to prevent.
    test "a live run is not a verdict" do
      assert {:ok, [], false, _state} = fetch([record("building")])
    end

    test "an interrupted attempt produces no verdict even beside completed runs" do
      interrupted =
        record("failed")
        |> JSON.decode!()
        |> Map.put("status", "interrupted")
        |> Map.put("interruption_reason", "the previous owner ended without a verdict")
        |> JSON.encode!()

      assert {:ok, [], false, _state} = fetch([interrupted])
      assert {:ok, [item], false, _state} = fetch([interrupted, record("passed")])
      assert item.verdict == :pass
    end

    test "a live run beside terminal ones is the only thing dropped" do
      assert {:ok, items, false, _state} =
               fetch([record("building"), record("passed"), record("failed")])

      assert Enum.map(items, & &1.verdict) == [:fail, :pass]
    end

    test "announcements are oldest first, so a batch reads in the order it happened" do
      assert {:ok, [first, second], false, _state} = fetch([record("passed"), record("failed")])

      assert first.id == "9d8c7b6a5f4e-1786543762957"
      assert second.id == "1f2e3d4c5b6a-1786546496920"
    end

    test "over the limit the newest survive and the overflow is reported" do
      assert {:ok, [only], true, _state} = fetch([record("passed"), record("failed")], 1)

      # Newest kept: a reader waiting on a verdict wants the freshest one, and
      # the dropped older ones are what the overflow line is for.
      assert only.id == "1f2e3d4c5b6a-1786546496920"
    end

    # Isolates the read cap specifically: the limit here is far above the
    # number of runs, so the terminal-count split cannot be what sets
    # overflow. Only the reader having stopped listing can. Asserting the
    # whole id list rather than a count also pins the oldest-first order at
    # the size where a reversed sort would still "pass" a count.
    test "a read that hit its own cap reports overflow even under the limit" do
      many = capped_records("cap", 1..40)

      assert {:ok, items, true, _state} = fetch(many, 100)
      assert Enum.map(items, & &1.id) == Enum.map(1..40, &"cap#{&1}-1786546496920")
    end

    test "a read comfortably under its cap does not cry overflow" do
      few = capped_records("few", 1..3)

      assert {:ok, items, false, _state} = fetch(few, 100)
      assert Enum.map(items, & &1.id) == Enum.map(1..3, &"few#{&1}-1786546496920")
    end

    # Fail closed: a read that could not answer must be distinguishable from a
    # read that answered "nothing new", because the watcher advances its
    # watermark on the second and not on the first.
    test "a failing read is an error, never a quiet window" do
      {:ok, state} =
        Verdicts.init(target: "/fixture/runs", read: fn _since -> {:error, "fixture outage"} end)

      assert {:error, "fixture outage"} = Verdicts.fetch(state, DateTime.utc_now(), 20)
    end

    test "ssh chatter on the shared stream is ignored rather than parsed" do
      lines = ["Warning: Permanently added a host key.", record("passed")]

      assert {:ok, [item], false, _state} = fetch(lines)
      assert item.verdict == :pass
    end

    test "a record caught mid-write is skipped without losing its neighbours" do
      truncated = String.slice(record("failed"), 0, 200)

      assert {:ok, [item], false, _state} = fetch([truncated, record("passed")])
      assert item.id == "1f2e3d4c5b6a-1786546496920"
    end

    test "a record missing its timestamps reports an unknown duration, not an instant one" do
      thin =
        record("passed")
        |> JSON.decode!()
        |> Map.delete("updated_at_ms")
        |> JSON.encode!()

      assert {:ok, [item], false, _state} = fetch([thin])
      assert item.duration_ms == nil
    end
  end

  describe "cadence and backfill" do
    test "the defaults are a minute and a ten-minute first look" do
      assert Verdicts.default_interval_ms() == 60_000
      assert Verdicts.initial_backfill_s() == 600
    end

    test "both are overridable, and a nonsense override falls back" do
      System.put_env("IX_MCP_FORGE_WATCH_INTERVAL_MS", "5000")
      System.put_env("IX_MCP_FORGE_WATCH_BACKFILL_S", "not-a-number")

      on_exit(fn ->
        System.delete_env("IX_MCP_FORGE_WATCH_INTERVAL_MS")
        System.delete_env("IX_MCP_FORGE_WATCH_BACKFILL_S")
      end)

      assert Verdicts.default_interval_ms() == 5_000
      assert Verdicts.initial_backfill_s() == 600
    end

    test "the feed names its own renderer" do
      assert Verdicts.renderer() == IxMcp.Forge.VerdictAnnounce
      assert Verdicts.label() == "forge"
    end
  end
end
