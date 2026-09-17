defmodule IxMcp.Stdlib.ForgeInterruptedTest do
  use ExUnit.Case, async: true

  alias IxMcp.Stdlib.Forge

  @tag capture_log: true
  test "the newest interrupted attempt returns without another read or a liveness probe" do
    for reason <- [nil, "", "the previous owner ended without a verdict"] do
      interrupted = %{
        "run_id" => "new-attempt",
        "change_id" => "0123456789abcdef",
        "status" => "interrupted",
        "interruption_reason" => reason,
        "started_at_ms" => 1_786_546_900_000,
        "updated_at_ms" => 1_786_546_940_000
      }

      passed = %{
        interrupted
        | "run_id" => "old-attempt",
          "status" => "passed",
          "started_at_ms" => 1_786_543_762_957,
          "updated_at_ms" => 1_786_544_150_957
      }

      owner = self()

      read = fn _since ->
        refute_received :read_interrupted
        send(owner, :read_interrupted)
        {:ok, Enum.map_join([passed, interrupted], "\n", &JSON.encode!/1)}
      end

      assert {:indeterminate, report} =
               Forge.await_verdict(
                 change_id: "0123456789abcdef",
                 runs: "/fixture/runs",
                 read: read,
                 consumer: fn -> flunk("interrupted attempt must not query liveness") end,
                 liveness_after_ms: 0,
                 timeout_ms: 1_000,
                 interval_ms: 0
               )

      assert_received :read_interrupted
      assert report.run_id == "new-attempt"
      assert report.status == "interrupted"
      assert report.reason =~ "interrupted without a verdict"
      refute Map.has_key?(report, :landed_commit)
      if is_binary(reason) and reason != "", do: assert(report.reason =~ reason)
    end
  end

  @tag capture_log: true
  test "reconciling an old interruption cannot hide a newer passed or building attempt" do
    old = %{
      "run_id" => "aaaaaaaaaaaa-1786543762957",
      "change_id" => "0123456789abcdef",
      "status" => "interrupted",
      "started_at_ms" => 1_786_543_762_957,
      "updated_at_ms" => 1_786_550_000_000
    }

    for status <- ["passed", "building"],
        missing_start <- [false, true],
        reverse <- [false, true] do
      newer = %{
        old
        | "run_id" => "bbbbbbbbbbbb-1786546900000",
          "status" => status,
          "started_at_ms" => 1_786_546_900_000,
          "updated_at_ms" => 1_786_546_940_000
      }

      records = [old, newer]

      records =
        if missing_start, do: Enum.map(records, &Map.delete(&1, "started_at_ms")), else: records

      records = if reverse, do: Enum.reverse(records), else: records

      result =
        Forge.await_verdict(
          change_id: "0123456789abcdef",
          runs: "/fixture/runs",
          read: fn _since -> {:ok, Enum.map_join(records, "\n", &JSON.encode!/1)} end,
          timeout_ms: 0,
          consumer: fn -> flunk("an immediate read must not query liveness") end
        )

      case status do
        "passed" ->
          assert {:passed, %{run_id: "bbbbbbbbbbbb-1786546900000"}} = result

        "building" ->
          assert {:indeterminate, %{reason: reason}} = result
          assert reason =~ "last recorded status: building"
          refute reason =~ "interrupted"
      end
    end
  end

  @tag capture_log: true
  test "updates within the same attempt still select its latest progress" do
    building = %{
      "run_id" => "bbbbbbbbbbbb-1786546900000",
      "change_id" => "0123456789abcdef",
      "status" => "building",
      "started_at_ms" => 1_786_546_900_000,
      "updated_at_ms" => 1_786_546_910_000
    }

    passed = %{building | "status" => "passed", "updated_at_ms" => 1_786_546_940_000}

    for records <- [[building, passed], [passed, building]] do
      assert {:passed, %{run_id: "bbbbbbbbbbbb-1786546900000"}} =
               Forge.await_verdict(
                 change_id: "0123456789abcdef",
                 runs: "/fixture/runs",
                 read: fn _since -> {:ok, Enum.map_join(records, "\n", &JSON.encode!/1)} end,
                 timeout_ms: 0
               )
    end
  end

  @tag capture_log: true
  test "legacy and fork progress remains execution-unverified through bounded polling" do
    owners = [
      %{value: nil, label: "legacy record without an owner"},
      %{value: %{"kind" => "unknown"}, label: "unknown owner"},
      %{value: %{"kind" => "fork"}, label: "fork owner"}
    ]

    for owner <- owners, status <- ["building", "materializing"] do
      record = %{
        "run_id" => "bbbbbbbbbbbb-1786546900000",
        "change_id" => "0123456789abcdef",
        "status" => status,
        "started_at_ms" => 1_786_546_900_000,
        "updated_at_ms" => 1_786_546_940_000
      }

      record = if owner.value, do: Map.put(record, "execution_owner", owner.value), else: record
      caller = self()

      assert {:indeterminate, report} =
               Forge.await_verdict(
                 change_id: "0123456789abcdef",
                 runs: "/fixture/runs",
                 read: fn _since -> {:ok, JSON.encode!(record)} end,
                 consumer: fn ->
                   send(caller, :consumer_checked)
                   {:dead, "inactive"}
                 end,
                 liveness_after_ms: 0,
                 timeout_ms: 1_000
               )

      assert_received :consumer_checked
      assert report.reason =~ "execution-unverified"
      assert report.reason =~ owner.label
      assert report.reason =~ "last recorded status: #{status}"
      assert report.reason =~ "no CI verdict is established"
      refute report.reason =~ "the run is #{status}"
      refute report.reason =~ "no verdict is coming"
      refute Map.has_key?(report, :landed_commit)
    end
  end
end
